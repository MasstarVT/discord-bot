//! Port of `src/dashboard/collections/automations.js`: the `automations` collection (When → If → Do rules), the tables
//! automationService has (restated, drift-tested in the JS), and the pure conversion between the stored
//! condition/action JSON and the form's shape.

use std::collections::HashMap;

use bot_config::constants::limits;
use bot_util::js::number_to_string;
use dash_schema::{RoleRef, ref_key};
use indexmap::IndexSet;
use serde_json::{Map, Value, json};

use crate::index::{CheckCtx, Mode, Problem};
use crate::util::{raw, strip_undefined, as_list, fmt_num, int_of, is_integer, iso, js_string, merged, non_empty, truthy};

/// `AUTOMATION_LIMITS`: limits of the automation normalisers.
pub mod automation_limits {
    pub const NAME: usize = 32;
    pub const CONTENT: usize = 2000;
    pub const THREAD_NAME: usize = 100;
    pub const BUTTON_LABEL: usize = 80;
    pub const BUTTON_URL: usize = 512;
    pub const BUTTON_INDEX: i64 = 99;
    pub const COOLDOWN_SECONDS: i64 = 604_800;
    pub const REWARD_AMOUNT: i64 = 1_000_000;
    pub const ID_LIST: usize = 25;
    pub const WORD: usize = 100;
    pub const EXACT_TEXT: usize = 500;
    pub const REACTED_EMOJIS: usize = 10;
    pub const REACTIONS_MIN: i64 = 1000;
    pub const ACTION_ROLES: usize = 10;
    pub const THREAD_USERS: usize = 10;
}

pub const THREAD_ARCHIVE_MINUTES: [i64; 4] = [60, 1440, 4320, 10080];
pub const DEFAULT_THREAD_NAME: &str = "{user.name}'s thread";
pub const WELCOMER_DEFAULT_TEXT: &str = "Hey {user}, welcome to **{server}**!";
pub const DEFAULT_STAR: &str = "⭐";
pub const DEFAULT_THRESHOLD: i64 = 3;
pub const BUTTON_STYLES: [&str; 4] = ["PRIMARY", "SECONDARY", "SUCCESS", "DANGER"];
/// The texts automationService and /automation show (ERROR_TEXTS).
pub const NEEDS_ACTIONS: &str = "Add at least one action before turning it on.";
pub const BUTTONS_USED: &str = "This automation has used all 99 button numbers. Create a new automation for more buttons.";

/// One trigger (`TRIGGER_RULES[type]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TriggerRule {
    pub label: &'static str,
    /// The trigger happens in a channel (the default target of a message).
    pub channel: bool,
    pub description: &'static str,
}

/// `TRIGGER_RULES`, in the JS order.
pub const TRIGGER_RULES: [(&str, TriggerRule); 11] = [
    ("MESSAGE_SENT", TriggerRule { label: "Message sent", channel: true, description: "A member sends a message (bots and webhooks never trigger automations)." }),
    ("MESSAGE_DELETED", TriggerRule { label: "Message deleted", channel: true, description: "A member's message is deleted (deletions by the bot do not count)." }),
    ("MESSAGE_EDITED", TriggerRule { label: "Message edited", channel: true, description: "A member edits a message." }),
    ("BUTTON_CLICKED", TriggerRule { label: "Button clicked", channel: true, description: "A member clicks a button that an automation posted." }),
    ("REACTION_ADDED", TriggerRule { label: "Reaction added", channel: true, description: "A member reacts to a message." }),
    ("ROLE_GAINED", TriggerRule { label: "Role gained", channel: false, description: "A member gets a role." }),
    ("ROLE_LOST", TriggerRule { label: "Role lost", channel: false, description: "A member loses a role." }),
    ("VOICE_JOINED", TriggerRule { label: "Joined voice", channel: true, description: "A member joins a voice channel (moving counts as leaving one and joining the other)." }),
    ("VOICE_LEFT", TriggerRule { label: "Left voice", channel: true, description: "A member leaves a voice channel." }),
    ("THREAD_CREATED", TriggerRule { label: "Thread created", channel: true, description: "A member creates a thread." }),
    ("MEMBER_JOINED", TriggerRule { label: "Member joined", channel: false, description: "A new member joins the server (bots do not count)." }),
];

/// `TRIGGER_RULES[type]`.
pub fn trigger_rule(t: &str) -> Option<&'static TriggerRule> {
    TRIGGER_RULES.iter().find(|(k, _)| *k == t).map(|(_, r)| r)
}

/// The trigger groups of automationService (`TRIGGER_SETS`).
pub mod trigger_sets {
    pub const ALL: [&str; 11] = [
        "MESSAGE_SENT", "MESSAGE_DELETED", "MESSAGE_EDITED", "BUTTON_CLICKED", "REACTION_ADDED", "ROLE_GAINED", "ROLE_LOST",
        "VOICE_JOINED", "VOICE_LEFT", "THREAD_CREATED", "MEMBER_JOINED",
    ];
    pub const CHANNEL: [&str; 8] =
        ["MESSAGE_SENT", "MESSAGE_DELETED", "MESSAGE_EDITED", "BUTTON_CLICKED", "REACTION_ADDED", "VOICE_JOINED", "VOICE_LEFT", "THREAD_CREATED"];
    pub const CONTENT: [&str; 4] = ["MESSAGE_SENT", "MESSAGE_DELETED", "MESSAGE_EDITED", "REACTION_ADDED"];
    /// The member's message still exists.
    pub const LIVE: [&str; 3] = ["MESSAGE_SENT", "MESSAGE_EDITED", "REACTION_ADDED"];
    /// Reaction counts mean something.
    pub const COUNTS: [&str; 3] = ["MESSAGE_DELETED", "MESSAGE_EDITED", "REACTION_ADDED"];
    pub const THREADY: [&str; 5] = ["THREAD_CREATED", "MESSAGE_SENT", "MESSAGE_EDITED", "REACTION_ADDED", "BUTTON_CLICKED"];
    pub const REPLYABLE: [&str; 4] = ["MESSAGE_SENT", "MESSAGE_EDITED", "REACTION_ADDED", "BUTTON_CLICKED"];
    pub const REACTION: [&str; 1] = ["REACTION_ADDED"];
    pub const ROLE_GAINED: [&str; 1] = ["ROLE_GAINED"];
    pub const ROLE_LOST: [&str; 1] = ["ROLE_LOST"];
    pub const BUTTON: [&str; 1] = ["BUTTON_CLICKED"];
}
use trigger_sets as s;

/// One condition type (`CONDITION_RULES[type]`): `field` is the form field holding its parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConditionRule {
    pub label: &'static str,
    pub triggers: &'static [&'static str],
    pub field: Option<&'static str>,
    pub no_params: bool,
}

const fn cond(label: &'static str, triggers: &'static [&'static str], field: &'static str) -> ConditionRule {
    ConditionRule { label, triggers, field: Some(field), no_params: false }
}

const fn flag_cond(label: &'static str, triggers: &'static [&'static str]) -> ConditionRule {
    ConditionRule { label, triggers, field: None, no_params: true }
}

/// `CONDITION_RULES`, in the JS order.
pub const CONDITION_RULES: [(&str, ConditionRule); 24] = [
    ("CHANNEL_IN", cond("Channel is one of", &s::CHANNEL, "channelIds")),
    ("CHANNEL_NOT_IN", cond("Channel is not one of", &s::CHANNEL, "channelIds")),
    ("CATEGORY_IN", cond("Category is one of", &s::CHANNEL, "categoryIds")),
    ("CATEGORY_NOT_IN", cond("Category is not one of", &s::CHANNEL, "categoryIds")),
    ("CONTAINS", cond("Message contains words", &s::CONTENT, "words")),
    ("EXACT", cond("Message is exactly", &s::CONTENT, "texts")),
    ("NOT_CONTAINS", cond("Message does not contain words", &s::CONTENT, "words")),
    ("MATCHES_REGEX", cond("Message matches a regex", &s::CONTENT, "pattern")),
    ("ATTACHMENT_AUDIO", flag_cond("Has an audio attachment", &s::CONTENT)),
    ("ATTACHMENT_IMAGE", flag_cond("Has an image attachment", &s::CONTENT)),
    ("ATTACHMENT_VIDEO", flag_cond("Has a video attachment", &s::CONTENT)),
    ("ATTACHMENT_TEXT", flag_cond("Has a text-file attachment", &s::CONTENT)),
    ("REACTED_WITH", cond("Reacted with emoji", &s::REACTION, "emojis")),
    ("HAS_REACTIONS", cond("Message has at least N reactions", &s::COUNTS, "min")),
    ("IS_REPLY", flag_cond("Message is a reply", &s::CONTENT)),
    ("IS_NOT_REPLY", flag_cond("Message is not a reply", &s::CONTENT)),
    ("HAS_ANY_ROLE", cond("Member has any of these roles", &s::ALL, "roleIds")),
    ("HAS_ALL_ROLES", cond("Member has all of these roles", &s::ALL, "roleIds")),
    ("HAS_NOT_ANY_ROLE", cond("Member has none of these roles", &s::ALL, "roleIds")),
    ("HAS_NOT_ALL_ROLES", cond("Member lacks one of these roles", &s::ALL, "roleIds")),
    ("GAINED_ROLE", cond("Gained one of these roles", &s::ROLE_GAINED, "roleIds")),
    ("LOST_ROLE", cond("Lost one of these roles", &s::ROLE_LOST, "roleIds")),
    ("USER_IS", cond("User is one of", &s::ALL, "users")),
    ("BUTTON_IS", cond("Clicked this button", &s::BUTTON, "buttons")),
];

/// `CONDITION_RULES[type]`.
pub fn condition_rule(t: &str) -> Option<&'static ConditionRule> {
    CONDITION_RULES.iter().find(|(k, _)| *k == t).map(|(_, r)| r)
}

/// One action type (`ACTION_RULES[type]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActionRule {
    pub label: &'static str,
    pub triggers: &'static [&'static str],
    /// The action posts a message that can carry a button.
    pub button: bool,
    /// The action needs a thread (a THREADY trigger or an earlier "Create a thread").
    pub thread: bool,
    /// 'coins' or 'xp' for the reward actions.
    pub reward: Option<&'static str>,
}

const fn act(label: &'static str, triggers: &'static [&'static str]) -> ActionRule {
    ActionRule { label, triggers, button: false, thread: false, reward: None }
}

/// `ACTION_RULES`, in the JS order.
pub const ACTION_RULES: [(&str, ActionRule); 17] = [
    ("CREATE_THREAD", act("Create a thread", &s::ALL)),
    ("SEND_MESSAGE", ActionRule { button: true, ..act("Send a message", &s::ALL) }),
    ("REPLY", ActionRule { button: true, ..act("Reply", &s::REPLYABLE) }),
    ("REPOST", act("Repost the message", &s::CONTENT)),
    ("PIN", act("Pin the message", &s::LIVE)),
    ("DELETE", act("Delete the message", &s::LIVE)),
    ("REACT", act("React with an emoji", &s::LIVE)),
    ("REMOVE_REACTION", act("Remove the reaction", &s::REACTION)),
    ("GIVE_ROLES", act("Give roles", &s::ALL)),
    ("TAKE_ROLES", act("Take roles", &s::ALL)),
    ("ADD_TO_THREAD", ActionRule { thread: true, ..act("Add members to the thread", &s::ALL) }),
    ("SEND_IN_THREAD", ActionRule { button: true, thread: true, ..act("Send a message in the thread", &s::ALL) }),
    ("GIVE_COINS", ActionRule { reward: Some("coins"), ..act("Give coins", &s::ALL) }),
    ("TAKE_COINS", ActionRule { reward: Some("coins"), ..act("Take coins", &s::ALL) }),
    ("GIVE_XP", ActionRule { reward: Some("xp"), ..act("Give XP", &s::ALL) }),
    ("TAKE_XP", ActionRule { reward: Some("xp"), ..act("Take XP", &s::ALL) }),
    ("SEND_EMBED", act("Send a saved embed", &s::ALL)),
];

/// `ACTION_RULES[type]`.
pub fn action_rule(t: &str) -> Option<&'static ActionRule> {
    ACTION_RULES.iter().find(|(k, _)| *k == t).map(|(_, r)| r)
}

/// /automation create presets (`PRESETS`).
pub const PRESETS: [&str; 4] = ["welcomer", "starboard", "thread-joiner", "commands-only"];
/// The options each preset reads (`PRESET_FIELDS`, form field names).
pub const PRESET_FIELDS: [(&str, &[&str]); 4] = [
    ("welcomer", &["presetChannelId", "presetMessage", "presetRoleId"]),
    ("starboard", &["presetChannelId", "presetEmoji", "presetThreshold"]),
    ("thread-joiner", &["presetUserId", "presetChannelId"]),
    ("commands-only", &["presetChannelId", "presetStaffRoleId"]),
];

/// Variables automation texts support besides the built-in ones (automationService.buildVars).
pub const AUTOMATION_VARIABLES: [&str; 15] = [
    "{automation}", "{message.content}", "{message.link}", "{message.id}", "{message.author}", "{message.author.name}",
    "{message.old_content}", "{reaction}", "{role}", "{role.id}", "{role.name}", "{voice}", "{voice.name}", "{thread}",
    "{thread.name}",
];

/// The automationId a create stores for a "Clicked this button" condition that points at the new automation itself.
pub const SELF_REF_ID: &str = "dashboardselfreference";

/// `problemKey(automationId)`: the Redis key of an automation's last runtime problem.
pub fn problem_key(automation_id: &str) -> String {
    format!("automation:problem:{automation_id}")
}

const ROLE_CONDITIONS: [&str; 6] = ["HAS_ANY_ROLE", "HAS_ALL_ROLES", "HAS_NOT_ANY_ROLE", "HAS_NOT_ALL_ROLES", "GAINED_ROLE", "LOST_ROLE"];
const MESSAGE_ACTIONS: [&str; 3] = ["SEND_MESSAGE", "REPLY", "SEND_IN_THREAD"];
const REWARD_ACTIONS: [&str; 4] = ["GIVE_COINS", "TAKE_COINS", "GIVE_XP", "TAKE_XP"];
const FLAG_ORDER: [&str; 4] = ["i", "m", "s", "u"];

fn type_str(v: &Value) -> Option<&str> {
    v.get("type").and_then(Value::as_str)
}

fn type_in(v: &Value, list: &[&str]) -> bool {
    type_str(v).is_some_and(|t| list.contains(&t))
}

/// `obj[key] = v` where `undefined` removes the key from the JSON (JSON.stringify drops it).
fn assign(m: &mut Map<String, Value>, key: &str, v: Option<&Value>) {
    match v {
        Some(v) => {
            m.insert(key.into(), v.clone());
        }
        None => {
            m.shift_remove(key);
        }
    }
}

fn type_value(t: Option<&Value>, default: &str) -> Option<Value> {
    match t {
        None => Some(json!(default)),
        Some(v) => Some(v.clone()),
    }
}

/// `blankCondition(type)`: a condition item with every field at its default (`type` undefined: 'CHANNEL_IN').
pub fn blank_condition(ty: Option<&Value>) -> Map<String, Value> {
    let mut m = Map::new();
    assign(&mut m, "type", type_value(ty, "CHANNEL_IN").as_ref());
    for (k, v) in [
        ("channelIds", json!([])),
        ("categoryIds", json!([])),
        ("words", json!([])),
        ("texts", json!([])),
        ("pattern", Value::Null),
        ("flags", json!(["i"])),
        ("emojis", json!([])),
        ("min", json!(DEFAULT_THRESHOLD)),
        ("emoji", Value::Null),
        ("roleIds", json!([])),
        ("users", json!([])),
        ("buttons", json!([])),
    ] {
        m.insert(k.into(), v);
    }
    m
}

fn blank_button() -> Value {
    json!({ "kind": "none", "label": null, "url": null, "style": "PRIMARY", "index": null })
}

/// `blankAction(type)`: an action item with every field at its default (`type` undefined: 'SEND_MESSAGE').
pub fn blank_action(ty: Option<&Value>) -> Map<String, Value> {
    let mut m = Map::new();
    assign(&mut m, "type", type_value(ty, "SEND_MESSAGE").as_ref());
    for (k, v) in [
        ("content", Value::Null),
        ("button", blank_button()),
        ("channelId", Value::Null),
        ("threadName", json!(DEFAULT_THREAD_NAME)),
        ("threadChannelId", Value::Null),
        ("autoArchiveMinutes", json!(1440)),
        ("emoji", Value::Null),
        ("roleIds", json!([])),
        ("users", json!([])),
        ("amount", json!(100)),
        ("embedName", Value::Null),
    ] {
        m.insert(k.into(), v);
    }
    m
}

fn flags_to_form(flags: Option<&Value>) -> Value {
    let text = match flags {
        None | Some(Value::Null) => String::new(),
        Some(v) => js_string(Some(v)),
    };
    Value::Array(FLAG_ORDER.iter().filter(|f| text.contains(**f)).map(|f| json!(f)).collect())
}

/// `flagsToStored(flags)`: the stored flags string, unique and sorted.
pub fn flags_to_stored(flags: Option<&Value>) -> String {
    let mut out: Vec<&str> = Vec::new();
    for f in as_list(flags) {
        if let Some(f) = f.as_str().filter(|f| FLAG_ORDER.contains(f))
            && !out.contains(&f)
        {
            out.push(f);
        }
    }
    out.sort_unstable();
    out.concat()
}

fn nn(v: Option<&Value>) -> Value {
    v.filter(|v| !v.is_null()).cloned().unwrap_or(Value::Null)
}

/// `conditionToForm(c, nameOf)`: one stored condition in the form's shape. `name_of`: the automation name of an id
/// (BUTTON_IS); unknown ids stay ids.
pub fn condition_to_form(c: &Value, name_of: &dyn Fn(&Value) -> Option<String>) -> Value {
    let mut f = blank_condition(c.get("type"));
    let list = |k: &str| Value::Array(as_list(c.get(k)).to_vec());
    match type_str(c) {
        Some("CHANNEL_IN" | "CHANNEL_NOT_IN") => {
            f.insert("channelIds".into(), list("channelIds"));
        }
        Some("CATEGORY_IN" | "CATEGORY_NOT_IN") => {
            f.insert("categoryIds".into(), list("categoryIds"));
        }
        Some("CONTAINS" | "NOT_CONTAINS") => {
            f.insert("words".into(), list("words"));
        }
        Some("EXACT") => {
            f.insert("texts".into(), list("texts"));
        }
        Some("MATCHES_REGEX") => {
            f.insert("pattern".into(), nn(c.get("pattern")));
            f.insert("flags".into(), flags_to_form(c.get("flags")));
        }
        Some("REACTED_WITH") => {
            let e: Vec<Value> = as_list(c.get("emojis")).iter().map(|e| json!({ "emoji": e })).collect();
            f.insert("emojis".into(), Value::Array(e));
        }
        Some("HAS_REACTIONS") => {
            assign(&mut f, "min", c.get("min"));
            f.insert("emoji".into(), nn(c.get("emoji")));
        }
        Some("USER_IS") => {
            let u: Vec<Value> = as_list(c.get("userIds")).iter().map(|id| json!({ "userId": id })).collect();
            f.insert("users".into(), Value::Array(u));
        }
        Some("BUTTON_IS") => {
            let b: Vec<Value> = as_list(c.get("buttons"))
                .iter()
                .map(|b| {
                    let id = b.get("automationId").cloned().unwrap_or(Value::Null);
                    let automation = name_of(&id).map_or(id, Value::String);
                    let mut m = Map::new();
                    m.insert("automation".into(), automation);
                    assign(&mut m, "index", b.get("index"));
                    Value::Object(m)
                })
                .collect();
            f.insert("buttons".into(), Value::Array(b));
        }
        _ => {
            if type_in(c, &ROLE_CONDITIONS) {
                f.insert("roleIds".into(), list("roleIds"));
            }
        }
    }
    Value::Object(f)
}

/// `conditionToStored(f, automationIdOf)`: one validated form condition in the stored shape. `automation_id_of`:
/// BUTTON_IS: the automation id a typed name stands for.
pub fn condition_to_stored(f: &Value, automation_id_of: &dyn Fn(&Value) -> Value) -> Value {
    let mut m = Map::new();
    assign(&mut m, "type", f.get("type"));
    let list = |k: &str| Value::Array(as_list(f.get(k)).to_vec());
    match type_str(f) {
        Some("CHANNEL_IN" | "CHANNEL_NOT_IN") => {
            m.insert("channelIds".into(), list("channelIds"));
        }
        Some("CATEGORY_IN" | "CATEGORY_NOT_IN") => {
            m.insert("categoryIds".into(), list("categoryIds"));
        }
        Some("CONTAINS" | "NOT_CONTAINS") => {
            m.insert("words".into(), list("words"));
        }
        Some("EXACT") => {
            m.insert("texts".into(), list("texts"));
        }
        Some("MATCHES_REGEX") => {
            m.insert("pattern".into(), f.get("pattern").filter(|v| !v.is_null()).cloned().unwrap_or(json!("")));
            m.insert("flags".into(), json!(flags_to_stored(f.get("flags"))));
        }
        Some("REACTED_WITH") => {
            m.insert("emojis".into(), Value::Array(as_list(f.get("emojis")).iter().map(|e| e.get("emoji").cloned().unwrap_or(Value::Null)).collect()));
        }
        Some("HAS_REACTIONS") => {
            assign(&mut m, "min", f.get("min"));
            m.insert("emoji".into(), nn(f.get("emoji")));
        }
        Some("USER_IS") => {
            m.insert("userIds".into(), Value::Array(as_list(f.get("users")).iter().map(|u| u.get("userId").cloned().unwrap_or(Value::Null)).collect()));
        }
        Some("BUTTON_IS") => {
            let b: Vec<Value> = as_list(f.get("buttons"))
                .iter()
                .map(|b| {
                    let mut x = Map::new();
                    x.insert("automationId".into(), automation_id_of(b.get("automation").unwrap_or(&Value::Null)));
                    assign(&mut x, "index", b.get("index"));
                    Value::Object(x)
                })
                .collect();
            m.insert("buttons".into(), Value::Array(b));
        }
        _ => {
            if type_in(f, &ROLE_CONDITIONS) {
                m.insert("roleIds".into(), list("roleIds"));
            }
        }
    }
    Value::Object(m)
}

fn button_to_form(b: Option<&Value>) -> Value {
    let Some(b) = b.filter(|b| truthy(Some(b))) else { return blank_button() };
    let mut m = Map::new();
    if b.get("url").is_some_and(|u| !u.is_null()) {
        m.insert("kind".into(), json!("link"));
        assign(&mut m, "label", b.get("label"));
        m.insert("url".into(), b["url"].clone());
        m.insert("style".into(), json!("PRIMARY"));
        m.insert("index".into(), Value::Null);
    } else {
        m.insert("kind".into(), json!("automation"));
        assign(&mut m, "label", b.get("label"));
        m.insert("url".into(), Value::Null);
        m.insert("style".into(), b.get("style").filter(|v| !v.is_null()).cloned().unwrap_or(json!("PRIMARY")));
        m.insert("index".into(), nn(b.get("index")));
    }
    Value::Object(m)
}

fn button_to_stored(b: Option<&Value>, index: Option<i64>) -> Value {
    let kind = b.and_then(|b| b.get("kind")).and_then(Value::as_str);
    let mut m = Map::new();
    match kind {
        Some("link") => {
            let b = b.unwrap_or(&Value::Null);
            assign(&mut m, "label", b.get("label"));
            assign(&mut m, "url", b.get("url"));
        }
        Some("automation") => {
            let b = b.unwrap_or(&Value::Null);
            assign(&mut m, "label", b.get("label"));
            m.insert("url".into(), Value::Null);
            m.insert("style".into(), b.get("style").filter(|v| !v.is_null()).cloned().unwrap_or(json!("PRIMARY")));
            let idx = match index {
                Some(i) => json!(i),
                None => nn(b.get("index")),
            };
            m.insert("index".into(), idx);
        }
        _ => return Value::Null,
    }
    Value::Object(m)
}

/// `actionToForm(a)`: one stored action (normalizeAction shape) in the form's shape.
pub fn action_to_form(a: &Value) -> Value {
    let mut f = blank_action(a.get("type"));
    match type_str(a) {
        Some("CREATE_THREAD") => {
            f.insert("threadName".into(), a.get("name").filter(|v| !v.is_null()).cloned().unwrap_or(json!(DEFAULT_THREAD_NAME)));
            f.insert("threadChannelId".into(), nn(a.get("channelId")));
            f.insert("autoArchiveMinutes".into(), a.get("autoArchiveMinutes").filter(|v| !v.is_null()).cloned().unwrap_or(json!(1440)));
        }
        Some("SEND_MESSAGE") => {
            assign(&mut f, "content", a.get("content"));
            f.insert("channelId".into(), nn(a.get("channelId")));
            f.insert("button".into(), button_to_form(a.get("button")));
        }
        Some("REPLY" | "SEND_IN_THREAD") => {
            assign(&mut f, "content", a.get("content"));
            f.insert("button".into(), button_to_form(a.get("button")));
        }
        Some("REPOST") => {
            f.insert("channelId".into(), nn(a.get("channelId")));
        }
        Some("REACT" | "REMOVE_REACTION") => {
            f.insert("emoji".into(), nn(a.get("emoji")));
        }
        Some("GIVE_ROLES" | "TAKE_ROLES") => {
            f.insert("roleIds".into(), Value::Array(as_list(a.get("roleIds")).to_vec()));
        }
        Some("ADD_TO_THREAD") => {
            f.insert("users".into(), Value::Array(as_list(a.get("userIds")).iter().map(|id| json!({ "userId": id })).collect()));
        }
        Some("SEND_EMBED") => {
            assign(&mut f, "embedName", a.get("embedName"));
            f.insert("channelId".into(), nn(a.get("channelId")));
        }
        _ => {
            if type_in(a, &REWARD_ACTIONS) {
                assign(&mut f, "amount", a.get("amount"));
            }
        }
    }
    Value::Object(f)
}

/// `actionToStored(f, buttonIndex)`: one validated form action in the stored shape. `button_index`: the number the
/// automation button gets (planButtons); None: the form's.
pub fn action_to_stored(f: &Value, button_index: Option<i64>) -> Value {
    let mut m = Map::new();
    assign(&mut m, "type", f.get("type"));
    match type_str(f) {
        Some("CREATE_THREAD") => {
            m.insert("name".into(), if non_empty(f.get("threadName")) { f["threadName"].clone() } else { Value::Null });
            m.insert("channelId".into(), nn(f.get("threadChannelId")));
            assign(&mut m, "autoArchiveMinutes", f.get("autoArchiveMinutes"));
        }
        Some("SEND_MESSAGE") => {
            m.insert("content".into(), f.get("content").filter(|v| !v.is_null()).cloned().unwrap_or(json!("")));
            m.insert("channelId".into(), nn(f.get("channelId")));
            m.insert("button".into(), button_to_stored(f.get("button"), button_index));
        }
        Some("REPLY" | "SEND_IN_THREAD") => {
            m.insert("content".into(), f.get("content").filter(|v| !v.is_null()).cloned().unwrap_or(json!("")));
            m.insert("button".into(), button_to_stored(f.get("button"), button_index));
        }
        Some("REPOST") => {
            m.insert("channelId".into(), nn(f.get("channelId")));
        }
        Some("REACT" | "REMOVE_REACTION") => {
            m.insert("emoji".into(), nn(f.get("emoji")));
        }
        Some("GIVE_ROLES" | "TAKE_ROLES") => {
            m.insert("roleIds".into(), Value::Array(as_list(f.get("roleIds")).to_vec()));
        }
        Some("ADD_TO_THREAD") => {
            m.insert("userIds".into(), Value::Array(as_list(f.get("users")).iter().map(|u| u.get("userId").cloned().unwrap_or(Value::Null)).collect()));
        }
        Some("SEND_EMBED") => {
            m.insert("embedName".into(), f.get("embedName").filter(|v| !v.is_null()).cloned().unwrap_or(json!("")));
            m.insert("channelId".into(), nn(f.get("channelId")));
        }
        _ => {
            if type_in(f, &REWARD_ACTIONS) {
                assign(&mut m, "amount", f.get("amount"));
            }
        }
    }
    Value::Object(m)
}

fn has_automation_button(f: &Value) -> bool {
    type_in(f, &MESSAGE_ACTIONS) && f.get("button").and_then(|b| b.get("kind")).and_then(Value::as_str) == Some("automation")
}

/// `a.button.index` when it is not null/undefined.
fn button_index(a: &Value) -> Option<&Value> {
    a.get("button").and_then(|b| b.get("index")).filter(|v| !v.is_null())
}

/// What [`plan_buttons`] answers: the number of each action's automation button (None: none, or not assigned) and the
/// row's new `buttonSeq`.
#[derive(Debug, Clone, PartialEq)]
pub struct ButtonPlan {
    pub indexes: Vec<Option<Value>>,
    pub button_seq: f64,
}

impl ButtonPlan {
    /// `{ indexes, buttonSeq }`.
    pub fn to_json(&self) -> Value {
        json!({ "indexes": self.indexes, "buttonSeq": dash_schema::js::js_num(self.button_seq) })
    }
}

/// `planButtons(actions, { existing, buttonSeq, assign })`: the numbers of the automation buttons of a form action
/// list, as /automation action add gives them. `existing`: the stored automation's button numbers; `assign` false only
/// checks the numbers that are given.
pub fn plan_buttons(actions: &[Value], existing: &[Value], button_seq: Option<&Value>, assign: bool) -> Result<ButtonPlan, Problem> {
    let have: Vec<f64> = existing.iter().filter_map(Value::as_f64).collect();
    let has = |v: &Value| v.as_f64().is_some_and(|x| have.contains(&x));
    let mut indexes: Vec<Option<Value>> = vec![None; actions.len()];
    let mut high = have.iter().copied().fold(0.0_f64, f64::max);
    let mut used: Vec<f64> = Vec::new();
    for (i, a) in actions.iter().enumerate() {
        if !has_automation_button(a) {
            continue;
        }
        let Some(idx) = button_index(a) else { continue };
        let text = js_string(Some(idx));
        if !has(idx) {
            return Err(Problem::new(
                format!("actions.{i}.button.index"),
                format!("Button number {text} is not one of this automation's buttons. Leave it empty to get a new number."),
            ));
        }
        let x = idx.as_f64().unwrap_or(f64::NAN);
        if used.contains(&x) {
            return Err(Problem::new(format!("actions.{i}.button.index"), format!("Action {}: button number {text} is used twice.", i + 1)));
        }
        used.push(x);
        indexes[i] = Some(idx.clone());
    }
    let mut seq = button_seq.filter(|v| is_integer(Some(v))).and_then(Value::as_f64).unwrap_or(0.0);
    for (i, a) in actions.iter().enumerate() {
        if !assign || !has_automation_button(a) || button_index(a).is_some() {
            continue;
        }
        let next = seq.max(high) + 1.0;
        if next > automation_limits::BUTTON_INDEX as f64 {
            return Err(Problem::new(format!("actions.{i}.button"), BUTTONS_USED));
        }
        indexes[i] = Some(dash_schema::js::js_num(next));
        seq = next;
        high = next;
    }
    Ok(ButtonPlan { indexes, button_seq: seq })
}

/// `shapeProblems({ trigger, conditions, actions, cooldownSeconds })`: automationService.validateAutomationShape on
/// stored shapes, with the field each problem is about (the service's texts word for word).
pub fn shape_problems(trigger: Option<&Value>, conditions: &[Value], actions: &[Value], cooldown_seconds: Option<&Value>) -> Vec<Problem> {
    let mut out = Vec::new();
    let Some((trigger, rule)) = trigger.and_then(Value::as_str).and_then(|t| trigger_rule(t).map(|r| (t, r))) else {
        return vec![Problem::new("trigger", "Unknown trigger.")];
    };
    let t_label = rule.label;
    if conditions.len() > limits::AUTOMATION_CONDITIONS {
        out.push(Problem::new("conditions", format!("An automation can have at most {} conditions.", limits::AUTOMATION_CONDITIONS)));
    }
    if actions.len() > limits::AUTOMATION_ACTIONS {
        out.push(Problem::new("actions", format!("An automation can have at most {} actions.", limits::AUTOMATION_ACTIONS)));
    }
    let cd_ok = int_of(cooldown_seconds).is_some_and(|c| (0..=automation_limits::COOLDOWN_SECONDS).contains(&c));
    if !cd_ok {
        out.push(Problem::new(
            "cooldownSeconds",
            format!("The cooldown must be a whole number of seconds from 0 to {}.", fmt_num(automation_limits::COOLDOWN_SECONDS as f64)),
        ));
    }

    let mut seen_flags: HashMap<&str, usize> = HashMap::new();
    for (k, c) in conditions.iter().enumerate() {
        let i = k + 1;
        let at = format!("conditions.{k}.type");
        let Some((ty, def)) = type_str(c).and_then(|t| condition_rule(t).map(|d| (t, d))) else {
            out.push(Problem::new(at, format!("Condition {i}: unknown type.")));
            continue;
        };
        if !def.triggers.contains(&trigger) {
            out.push(Problem::new(at.clone(), format!("Condition {i}: \"{}\" doesn't work with the trigger \"{t_label}\".", def.label)));
        }
        if def.no_params {
            if seen_flags.contains_key(ty) {
                out.push(Problem::new(at, format!("Condition {i} is already there.")));
            } else {
                seen_flags.insert(ty, k);
            }
        }
    }
    if let (Some(a), Some(b)) = (seen_flags.get("IS_REPLY"), seen_flags.get("IS_NOT_REPLY")) {
        out.push(Problem::new(
            format!("conditions.{}.type", (*a).max(*b)),
            "These two conditions can never both be true: \"Message is a reply\" and \"Message is not a reply\".",
        ));
    }

    let mut used_buttons: Vec<f64> = Vec::new();
    let mut thread_before = false;
    for (k, a) in actions.iter().enumerate() {
        let i = k + 1;
        let at = format!("actions.{k}");
        let Some((ty, def)) = type_str(a).and_then(|t| action_rule(t).map(|d| (t, d))) else {
            out.push(Problem::new(format!("{at}.type"), format!("Action {i}: unknown type.")));
            continue;
        };
        if !def.triggers.contains(&trigger) {
            out.push(Problem::new(format!("{at}.type"), format!("Action {i}: \"{}\" doesn't work with the trigger \"{t_label}\".", def.label)));
        }
        let channel = truthy(a.get("channelId"));
        if (ty == "SEND_MESSAGE" || ty == "SEND_EMBED") && !channel && !s::CHANNEL.contains(&trigger) {
            out.push(Problem::new(format!("{at}.channelId"), format!("Action {i}: this trigger doesn't happen in a channel, so pick a channel.")));
        }
        if ty == "CREATE_THREAD" && !channel && !s::LIVE.contains(&trigger) {
            out.push(Problem::new(format!("{at}.channelId"), format!("Action {i}: there is no message to start a thread from, so pick a channel.")));
        }
        if def.thread && !s::THREADY.contains(&trigger) && !thread_before {
            out.push(Problem::new(format!("{at}.type"), format!("Action {i}: there is no thread here. Add \"Create a thread\" before it.")));
        }
        if ty == "CREATE_THREAD" {
            thread_before = true;
        }
        if let Some(b) = a.get("button").filter(|b| truthy(Some(b)))
            && b.get("url").is_none_or(Value::is_null)
            && is_integer(b.get("index"))
        {
            let x = b["index"].as_f64().unwrap_or(f64::NAN);
            if used_buttons.contains(&x) {
                out.push(Problem::new(
                    format!("{at}.button.index"),
                    format!("Action {i}: button number {} is used twice.", number_to_string(x)),
                ));
            }
            used_buttons.push(x);
        }
    }
    out
}

/// The problem with one form condition's parameters (field, message).
fn condition_param_problem(f: &Value) -> Option<(&'static str, String)> {
    let empty = |k: &str| as_list(f.get(k)).is_empty();
    match type_str(f) {
        Some("CHANNEL_IN" | "CHANNEL_NOT_IN") => empty("channelIds").then(|| ("channelIds", "Pick at least one channel.".into())),
        Some("CATEGORY_IN" | "CATEGORY_NOT_IN") => empty("categoryIds").then(|| ("categoryIds", "Pick at least one category.".into())),
        Some("CONTAINS" | "NOT_CONTAINS") => empty("words").then(|| ("words", "Add at least one word.".into())),
        Some("EXACT") => empty("texts").then(|| ("texts", "Add at least one text.".into())),
        Some("MATCHES_REGEX") => {
            let Some(p) = f.get("pattern").and_then(Value::as_str).filter(|p| !p.is_empty()) else {
                return Some(("pattern", "Write the regular expression.".into()));
            };
            bot_util::safe_regex::validate_regex(p, &flags_to_stored(f.get("flags"))).err().map(|r| ("pattern", format!("Invalid regex: {r}")))
        }
        Some("REACTED_WITH") => empty("emojis").then(|| ("emojis", "Add at least one emoji.".into())),
        Some("USER_IS") => empty("users").then(|| ("users", "Add at least one member.".into())),
        Some("BUTTON_IS") => empty("buttons").then(|| ("buttons", "Add at least one button.".into())),
        _ => (type_in(f, &ROLE_CONDITIONS) && empty("roleIds")).then(|| ("roleIds", "Pick at least one role.".into())),
    }
}

/// The problem with one form action's parameters (field, message).
fn action_param_problem(f: &Value) -> Option<(&'static str, &'static str)> {
    if type_in(f, &MESSAGE_ACTIONS) {
        if !non_empty(f.get("content")) {
            return Some(("content", "Write the message."));
        }
        let b = f.get("button").filter(|b| !b.is_null());
        let kind = b.and_then(|b| b.get("kind")).and_then(Value::as_str);
        if matches!(kind, Some("link" | "automation")) {
            let b = b.unwrap_or(&Value::Null);
            if !non_empty(b.get("label")) {
                return Some(("button.label", "Give the button a label."));
            }
            if kind == Some("link") && !non_empty(b.get("url")) {
                return Some(("button.url", "Add the link the button opens."));
            }
        }
        return None;
    }
    match type_str(f) {
        Some("REPOST") => (!truthy(f.get("channelId"))).then_some(("channelId", "Pick the channel to repost to.")),
        Some("REACT") => (!truthy(f.get("emoji"))).then_some(("emoji", "Pick the emoji.")),
        Some("GIVE_ROLES" | "TAKE_ROLES") => as_list(f.get("roleIds")).is_empty().then_some(("roleIds", "Pick at least one role.")),
        Some("SEND_EMBED") => (!non_empty(f.get("embedName"))).then_some(("embedName", "Type the name of the saved embed to send.")),
        _ => None,
    }
}

/// Form actions reduced to what shapeProblems reads (type, target channel, automation button).
fn shape_action(f: &Value) -> Value {
    let mut m = Map::new();
    assign(&mut m, "type", f.get("type"));
    let channel = if type_str(f) == Some("CREATE_THREAD") { f.get("threadChannelId") } else { f.get("channelId") };
    assign(&mut m, "channelId", channel);
    let button = if has_automation_button(f) {
        let mut b = Map::new();
        b.insert("url".into(), Value::Null);
        assign(&mut b, "index", f.get("button").and_then(|b| b.get("index")));
        Value::Object(b)
    } else {
        Value::Null
    };
    m.insert("button".into(), button);
    Value::Object(m)
}

/// `checkAutomation(data, { mode, current })`: cross-field rules of an automation.
pub fn check_automation(data: &Map<String, Value>, ctx: &CheckCtx) -> Option<Problem> {
    if ctx.mode != Mode::Update && truthy(data.get("preset")) && data.get("preset").and_then(Value::as_str) != Some("none") {
        return check_preset(data);
    }
    let t = if ctx.mode == Mode::Update && ctx.current.is_some() { merged(ctx.current, data) } else { data.clone() };
    let conditions = as_list(t.get("conditions"));
    let actions = as_list(t.get("actions"));
    for (i, c) in conditions.iter().enumerate() {
        if let Some((field, message)) = condition_param_problem(c) {
            return Some(Problem::new(format!("conditions.{i}.{field}"), message));
        }
    }
    for (i, a) in actions.iter().enumerate() {
        if let Some((field, message)) = action_param_problem(a) {
            return Some(Problem::new(format!("actions.{i}.{field}"), message));
        }
    }
    let existing: Vec<Value> = match (ctx.mode, ctx.current) {
        (Mode::Update, Some(current)) => as_list(current.get("actions"))
            .iter()
            .filter(|a| has_automation_button(a))
            .filter_map(|a| a.get("button").and_then(|b| b.get("index")).filter(|i| is_integer(Some(i))).cloned())
            .collect(),
        _ => Vec::new(),
    };
    if let Err(p) = plan_buttons(actions, &existing, None, false) {
        return Some(p);
    }
    let shaped: Vec<Value> = actions.iter().map(shape_action).collect();
    let cooldown = t.get("cooldownSeconds").filter(|v| !v.is_null()).cloned().unwrap_or(json!(0));
    let shape = shape_problems(t.get("trigger"), conditions, &shaped, Some(&cooldown));
    if let Some(p) = shape.into_iter().next() {
        let path = p
            .path
            .strip_prefix("actions.")
            .and_then(|rest| rest.strip_suffix(".channelId"))
            .filter(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
            .and_then(|n| n.parse::<usize>().ok().map(|i| (n, i)))
            .filter(|(_, i)| actions.get(*i).and_then(type_str) == Some("CREATE_THREAD"))
            .map_or(p.path.clone(), |(n, _)| format!("actions.{n}.threadChannelId"));
        return Some(Problem::new(path, p.message));
    }
    let enabled_true = data.get("enabled") == Some(&Value::Bool(true));
    let turning_on = if ctx.mode == Mode::Update {
        enabled_true && ctx.current.and_then(|c| c.get("enabled")) != Some(&Value::Bool(true))
    } else {
        t.get("enabled") == Some(&Value::Bool(true))
    };
    let keeps_on = ctx.mode == Mode::Update && enabled_true && data.contains_key("actions");
    if (turning_on || keeps_on) && actions.is_empty() {
        return Some(Problem::new("enabled", NEEDS_ACTIONS));
    }
    None
}

/// The /automation create preset options each preset needs.
fn check_preset(data: &Map<String, Value>) -> Option<Problem> {
    let has = |k: &str| truthy(data.get(k));
    match data.get("preset").and_then(Value::as_str) {
        Some("welcomer") => (!has("presetChannelId")).then(|| Problem::new("presetChannelId", "The Welcomer preset needs a channel to post in.")),
        Some("starboard") => (!has("presetChannelId")).then(|| Problem::new("presetChannelId", "The Starboard preset needs a channel to repost to.")),
        Some("thread-joiner") => {
            (!has("presetUserId")).then(|| Problem::new("presetUserId", "The Thread Joiner preset needs a member to add to new threads."))
        }
        Some("commands-only") => {
            (!has("presetChannelId")).then(|| Problem::new("presetChannelId", "The Commands Only preset needs the channel to keep clean."))
        }
        _ => Some(Problem::new("preset", "Pick a preset from the list.")),
    }
}

fn plural(n: usize, word: &str) -> String {
    format!("{n} {word}{}", if n == 1 { "" } else { "s" })
}

/// `summaryOf(row)`: the list line of an automation (as /automation list).
pub fn summary_of(row: &Value) -> String {
    let trigger = row.get("trigger");
    let label = trigger.and_then(Value::as_str).and_then(trigger_rule).map_or_else(
        || match trigger {
            None | Some(Value::Null) => String::new(),
            Some(v) => js_string(Some(v)),
        },
        |r| r.label.to_string(),
    );
    let conds = as_list(row.get("conditions")).len();
    let acts = as_list(row.get("actions")).len();
    format!("{label} · {} · {}", if conds > 0 { plural(conds, "condition") } else { "always".into() }, plural(acts, "action"))
}

/// `automationItem(row, { nameOf, problems, creator })`: an Automation row (conditions and actions already normalised)
/// as a collection item. `creator`: the display name of row.createdBy (default: the id).
pub fn automation_item(row: &Value, name_of: &dyn Fn(&Value) -> Option<String>, problems: &[String], creator: Option<&str>) -> Value {
    let created_by = match creator {
        Some(c) => json!(c),
        None => row.get("createdBy").and_then(Value::as_str).filter(|s| crate::util::is_snowflake(s)).map_or(Value::Null, |s| json!(s)),
    };
    strip_undefined(json!({
        "id": raw(row.get("id")),
        "name": raw(row.get("name")),
        "preset": "none",
        "presetChannelId": null,
        "presetMessage": null,
        "presetRoleId": null,
        "presetEmoji": null,
        "presetThreshold": DEFAULT_THRESHOLD,
        "presetUserId": null,
        "presetStaffRoleId": null,
        "trigger": raw(row.get("trigger")),
        "conditions": as_list(row.get("conditions")).iter().map(|c| condition_to_form(c, name_of)).collect::<Vec<_>>(),
        "actions": as_list(row.get("actions")).iter().map(action_to_form).collect::<Vec<_>>(),
        "cooldownSeconds": int_of(row.get("cooldownSeconds")).unwrap_or(0),
        "enabled": row.get("enabled") == Some(&Value::Bool(true)),
        "summary": summary_of(row),
        "uses": int_of(row.get("uses")).unwrap_or(0),
        "problems": problems,
        "createdBy": created_by,
        "createdAt": iso(row.get("createdAt")),
        "updatedAt": iso(row.get("updatedAt")),
    }))
}

fn action_role_key(ty: Option<&Value>, id: &str) -> String {
    format!("actions.{}.roleIds|{id}", js_string(ty))
}

/// `refKeys` of automations (`actionRoleKeys`): the generic stored keys, with action roles keyed by their action's
/// type.
pub fn action_role_keys(item: Option<&Value>, keys: IndexSet<String>) -> IndexSet<String> {
    let mut out: IndexSet<String> = keys.into_iter().filter(|k| !k.starts_with("actions.roleIds|")).collect();
    for action in as_list(item.and_then(|i| i.get("actions"))) {
        for id in as_list(action.get("roleIds")) {
            out.insert(action_role_key(action.get("type"), &js_string(Some(id))));
        }
    }
    out
}

/// `refKeyOf` of automations (`actionRoleKeyOf`): an action role is keyed by the type that action has in the request.
pub fn action_role_key_of(r: &RoleRef, data: &Map<String, Value>) -> String {
    let parts: Vec<&str> = r.path.split('.').collect();
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if parts.len() == 4 && parts[0] == "actions" && digits(parts[1]) && parts[2] == "roleIds" && digits(parts[3]) {
        let n: usize = parts[1].parse().unwrap_or(usize::MAX);
        let ty = data.get("actions").and_then(Value::as_array).and_then(|a| a.get(n)).and_then(|a| a.get("type"));
        return action_role_key(ty, &r.id);
    }
    ref_key(&r.path, &r.id)
}
