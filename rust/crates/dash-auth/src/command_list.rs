//! Port of `src/dashboard/commandList.js`: the public command reference (/docs), a plain summary of every slash
//! command, built from the command payloads (`scripts/build-command-list.js` keeps it in `src/dashboard/commands.json`).

use std::cmp::Ordering;

use bot_config::constants::PermissionLevel;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One subcommand (groups flattened to `"group sub"`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubcommandSummary {
    pub name: String,
    pub description: String,
}

/// One command of the reference (an entry of `src/dashboard/commands.json`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandSummary {
    pub name: String,
    pub description: String,
    pub module: Option<String>,
    /// `everyone`, `moderators`, `admins` or `owner`.
    pub who: String,
    pub subcommands: Vec<SubcommandSummary>,
}

/// Who can run a command, by permission level (a command without one is open to everyone).
pub fn who_of(level: Option<PermissionLevel>) -> &'static str {
    match level.unwrap_or(PermissionLevel::Member) {
        PermissionLevel::Member => "everyone",
        PermissionLevel::Moderator => "moderators",
        PermissionLevel::Admin => "admins",
        PermissionLevel::Owner => "owner",
    }
}

const SUB_COMMAND: u64 = 1;
const SUB_COMMAND_GROUP: u64 = 2;

fn str_of(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or_default().to_string()
}

fn options_of(v: &Value) -> &[Value] {
    v.get("options").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[])
}

/// The subcommands of a command payload, groups flattened to "group sub", in declaration order.
pub fn subcommands(payload: &Value) -> Vec<SubcommandSummary> {
    options_of(payload)
        .iter()
        .flat_map(|opt| match opt.get("type").and_then(Value::as_u64) {
            Some(SUB_COMMAND) => vec![SubcommandSummary { name: str_of(opt, "name"), description: str_of(opt, "description") }],
            Some(SUB_COMMAND_GROUP) => options_of(opt)
                .iter()
                .map(|sub| SubcommandSummary {
                    name: format!("{} {}", str_of(opt, "name"), str_of(sub, "name")),
                    description: str_of(sub, "description"),
                })
                .collect(),
            _ => Vec::new(),
        })
        .collect()
}

/// ICU root collation (what `localeCompare` uses) for the characters of command names: `_` before `-`, both before
/// digits, digits before letters, letters case-insensitively at the first level with lower case first. Anything
/// else ranks after these, by code point.
fn collation_key(c: char) -> (u32, u32) {
    match c {
        '_' => (0, 0),
        '-' => (1, 0),
        '0'..='9' => (10 + c as u32 - '0' as u32, 0),
        'a'..='z' => (100 + c as u32 - 'a' as u32, 0),
        'A'..='Z' => (100 + c as u32 - 'A' as u32, 1),
        _ => (1000 + c as u32, 0),
    }
}

/// The three ICU levels of a string: base characters, accents (combining marks after NFD), case.
fn collation_levels(s: &str) -> (Vec<u32>, Vec<u32>, Vec<u32>) {
    use unicode_normalization::char::is_combining_mark;
    use unicode_normalization::UnicodeNormalization;
    let (mut primary, mut secondary, mut tertiary) = (Vec::new(), Vec::new(), Vec::new());
    for c in s.nfd() {
        if is_combining_mark(c) && !secondary.is_empty() {
            if let Some(last) = secondary.last_mut() {
                *last = c as u32;
            }
            continue;
        }
        let (p, t) = collation_key(c);
        primary.push(p);
        secondary.push(0);
        tertiary.push(t);
    }
    (primary, secondary, tertiary)
}

/// `a.localeCompare(b)` for command names.
pub fn command_name_compare(a: &str, b: &str) -> Ordering {
    let (pa, sa, ta) = collation_levels(a);
    let (pb, sb, tb) = collation_levels(b);
    pa.cmp(&pb).then_with(|| sa.cmp(&sb)).then_with(|| ta.cmp(&tb))
}

/// One loaded command file: its routing metadata (`ns.module`, `ns.permissionLevel`) and its payload.
#[derive(Debug, Clone)]
pub struct CommandEntry<'a> {
    pub module: Option<&'a str>,
    pub permission_level: Option<PermissionLevel>,
    pub payload: &'a Value,
}

/// Summarises loaded command files, sorted by name (`summarizeCommands`).
pub fn summarize_commands(entries: &[CommandEntry<'_>]) -> Vec<CommandSummary> {
    let mut out: Vec<CommandSummary> = entries
        .iter()
        .map(|e| CommandSummary {
            name: str_of(e.payload, "name"),
            description: str_of(e.payload, "description"),
            module: e.module.map(str::to_string),
            who: who_of(e.permission_level).to_string(),
            subcommands: subcommands(e.payload),
        })
        .collect();
    out.sort_by(|a, b| command_name_compare(&a.name, &b.name));
    out
}
