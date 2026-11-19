//! The collections whose JS files hold constants and a `check` only:
//! - `src/dashboard/collections/levelRoles.js` (level rewards),
//! - `shopItems.js`, `economyBoosts.js` (Economy),
//! - `achievements.js`,
//! - `starboards.js`,
//! - `tempVoiceHubs.js`,
//! - `statChannels.js`,
//! - `translateChannels.js`.

use std::sync::LazyLock;

use bot_config::constants::{XP_MAX, xp_for_level};
use dash_schema::{Field, Refs, is_snowflake, validate_value};
use serde_json::{Map, Value};

use crate::index::{CheckCtx, Mode, Problem};
use crate::util::{as_list, fmt_num, is_integer, js_string, merged, slice16, strict_eq, truthy};

// ── levelRoles.js ─────────────────────────────────────────────────────────────────────────────────────────────

/// /levelconfig role add accepts levels 1 to 999.
pub const LEVEL_INPUT_MAX: i64 = 999;

/// `totalXpForLevel(level)`: total XP needed to reach `level` (the same sum as xpService).
pub fn total_xp_for_level(level: i64) -> i64 {
    let mut total: i64 = 0;
    for i in 0..level.max(0) {
        total = total.saturating_add(xp_for_level(i));
    }
    total
}

/// `highestReachableLevel()`: the highest level whose total XP still fits in XP_MAX.
pub fn highest_reachable_level() -> i64 {
    let mut level = 0;
    let mut total: i64 = 0;
    while total + xp_for_level(level) <= i64::from(XP_MAX) {
        total += xp_for_level(level);
        level += 1;
    }
    level
}

/// The `check` of levelRoles: levels nobody can reach (more XP than the cap) are refused.
pub fn check_level_role(data: &Map<String, Value>, ctx: &CheckCtx) -> Option<Problem> {
    let level = match data.get("level").filter(|v| !v.is_null()) {
        Some(v) => Some(v),
        None => ctx.current.and_then(|c| c.get("level")),
    };
    if !is_integer(level) {
        return None;
    }
    let lv = level.and_then(Value::as_f64).unwrap_or(0.0);
    // Levels are at most 999 here; a stored level above i64 range cannot occur.
    if total_xp_for_level(lv as i64) > i64::from(XP_MAX) {
        return Some(Problem::new(
            "level",
            format!(
                "Level {} needs more than {} XP, so nobody can reach it. The highest reachable level is {}.",
                js_string(level),
                fmt_num(f64::from(XP_MAX)),
                highest_reachable_level()
            ),
        ));
    }
    None
}

// ── shopItems.js ──────────────────────────────────────────────────────────────────────────────────────────────

pub const ITEM_NAME_MAX: usize = 50;
pub const ITEM_DESCRIPTION_MAX: usize = 200;
pub const ITEM_MESSAGE_MAX: usize = 1000;
pub const ITEM_IMAGE_MAX: usize = 500;
pub const MAX_STOCK: i64 = 1_000_000;
/// Variables the buy and use messages understand (itemVars in economyService.js).
pub const ITEM_MESSAGE_TOKENS: [&str; 6] = ["{item}", "{quantity}", "{coins}", "{price}", "{currency}", "{currency.emoji}"];

fn update_merged(data: &Map<String, Value>, ctx: &CheckCtx) -> Map<String, Value> {
    if ctx.mode == Mode::Update { merged(ctx.current, data) } else { data.clone() }
}

/// The `check` of shopItems: the cross-field rules of /economy-admin item create and edit.
pub fn check_shop_item(data: &Map<String, Value>, ctx: &CheckCtx) -> Option<Problem> {
    let m = update_merged(data, ctx);
    let name = match data.get("name") {
        None | Some(Value::Null) => String::new(),
        Some(v) => js_string(Some(v)),
    };
    if data.contains_key("name") && bot_util::js::trim(&name).to_lowercase() == "none" {
        return Some(Problem::new("name", "Pick a different name."));
    }
    let ty = m.get("type").and_then(Value::as_str);
    if ty == Some("ROLE") && !truthy(m.get("roleId")) {
        return Some(Problem::new("roleId", "A Role item needs a role."));
    }
    if ty != Some("ROLE") && truthy(data.get("roleId")) {
        return Some(Problem::new("roleId", "Only Role items can have a role. Set the type to Role as well."));
    }
    if ty == Some("STATIC") && m.get("useOnBuy") == Some(&Value::Bool(true)) {
        return Some(Problem::new("useOnBuy", "Collectible items can't be used, so they can't be used on purchase."));
    }
    if ctx.mode == Mode::Create && data.get("stock").and_then(Value::as_f64) == Some(0.0) {
        return Some(Problem::new("stock", "Start with a stock of at least 1, or leave it empty for unlimited. You can set 0 later."));
    }
    None
}

// ── economyBoosts.js ──────────────────────────────────────────────────────────────────────────────────────────

pub const BOOST_NAME_MAX: usize = 32;
pub const BOOST_PERCENT_MAX: i64 = 500;

// ── achievements.js ───────────────────────────────────────────────────────────────────────────────────────────

pub const NAME_MAX: usize = 50;
pub const DESCRIPTION_MAX: usize = 200;
pub const GOAL_INPUT_MAX: i64 = 10_000_000;
pub const MAX_REWARD_AMOUNT: i64 = 1_000_000;
pub const DEADLINE_MIN_MS: f64 = 60_000.0;
pub const DEADLINE_MAX_MS: f64 = 5.0 * 365.0 * 86_400_000.0;

/// `GOAL_MAX`: highest goal per trigger.
pub const GOAL_MAX: [(&str, i64); 5] =
    [("MESSAGES", 10_000_000), ("REACTIONS", 10_000_000), ("LEVEL", 1000), ("BOOST", 100), ("PURCHASES", 1_000_000)];
/// `TRIGGER_LABELS`.
pub const TRIGGER_LABELS: [(&str, &str); 5] = [
    ("MESSAGES", "Messages sent"),
    ("REACTIONS", "Reactions added"),
    ("LEVEL", "Level reached"),
    ("BOOST", "Server boosts"),
    ("PURCHASES", "Shop items bought"),
];
/// `TIER_NAMES`.
pub const TIER_NAMES: [&str; 4] = ["Bronze", "Silver", "Gold", "Diamond"];

/// `TRIGGER_LABELS[trigger]`.
pub fn trigger_label(trigger: &str) -> Option<&'static str> {
    TRIGGER_LABELS.iter().find(|(k, _)| *k == trigger).map(|(_, v)| *v)
}

/// A JS relational comparison `a > b` / `a <= b` operand.
fn num(v: Option<&Value>) -> f64 {
    match v {
        None => f64::NAN,
        Some(Value::Null) => 0.0,
        Some(v) => dash_schema::js::to_number(Some(v)),
    }
}

fn check_tiers(tiers: &[Value], trigger: Option<&Value>) -> Option<Problem> {
    let trig = trigger.and_then(Value::as_str);
    let max = trig.and_then(|t| GOAL_MAX.iter().find(|(k, _)| *k == t)).map(|(_, m)| *m);
    let mut previous = 0.0;
    for (i, tier) in tiers.iter().enumerate() {
        let label = TIER_NAMES.get(i).map_or_else(|| format!("Tier {}", i + 1), |s| (*s).to_string());
        let goal = num(tier.get("goal"));
        if let Some(max) = max
            && goal > max as f64
        {
            return Some(Problem::new(
                format!("tiers.{i}.goal"),
                format!("{label}: {} goals can be at most {}.", trigger_label(trig.unwrap_or("")).unwrap_or("undefined"), fmt_num(max as f64)),
            ));
        }
        if goal <= previous {
            return Some(Problem::new(
                format!("tiers.{i}.goal"),
                format!("{label}: goals must go up from tier to tier (this one has to be above {}).", fmt_num(previous)),
            ));
        }
        previous = goal;
        let add = tier.get("roleAdd");
        if truthy(add) && strict_eq(add.unwrap_or(&Value::Null), tier.get("roleRemove").unwrap_or(&Value::Null)) {
            return Some(Problem::new(format!("tiers.{i}.roleRemove"), format!("{label}: a tier cannot give and take the same role.")));
        }
    }
    None
}

fn check_deadline(deadline: &Value, now_ms: f64) -> Option<Problem> {
    let t = deadline.as_str().map_or(f64::NAN, bot_util::js::date_parse);
    let delta = t - now_ms;
    if delta < DEADLINE_MIN_MS {
        return Some(Problem::new("deadline", "The deadline must be in the future."));
    }
    if delta > DEADLINE_MAX_MS {
        return Some(Problem::new("deadline", "The deadline can be at most 5 years away."));
    }
    None
}

/// The `check` of achievements: goals that go up and fit the trigger, a tier never gives and takes the same role,
/// a deadline between a minute and five years ahead (an unchanged one stays valid).
pub fn check_achievement(data: &Map<String, Value>, ctx: &CheckCtx) -> Option<Problem> {
    let trigger = match data.get("trigger").filter(|v| !v.is_null()) {
        Some(t) => Some(t),
        None => ctx.current.and_then(|c| c.get("trigger")),
    };
    if truthy(data.get("tiers"))
        && let Some(p) = check_tiers(as_list(data.get("tiers")), trigger)
    {
        return Some(p);
    }
    if let Some(deadline) = data.get("deadline").filter(|d| truthy(Some(d))) {
        let parse = |v: Option<&Value>| v.and_then(Value::as_str).map_or(f64::NAN, bot_util::js::date_parse);
        let unchanged = ctx.mode == Mode::Update
            && ctx.current.is_some_and(|c| truthy(c.get("deadline")))
            && parse(ctx.current.and_then(|c| c.get("deadline"))) == parse(Some(deadline));
        if !unchanged
            && let Some(p) = check_deadline(deadline, ctx.now_ms)
        {
            return Some(p);
        }
    }
    None
}

// ── starboards.js ─────────────────────────────────────────────────────────────────────────────────────────────

pub const MAX_EMOJIS_PER_BOARD: usize = 10;
pub const MAX_IGNORED_CHANNELS: usize = 50;
pub const BOARD_NAME_MAX: usize = 32;

static EMOJI_FIELD: LazyLock<Field> =
    LazyLock::new(|| Field::from_json(serde_json::json!({ "type": "emoji", "label": "Emoji" })).expect("an object"));

/// `isEmojiEntry(entry)`: a unicode emoji, a custom emoji `<:name:id>` or the bare id of one.
pub fn is_emoji_entry(entry: &Value) -> bool {
    is_snowflake(entry) || validate_value(&EMOJI_FIELD, entry, Some("emojis"), &mut Refs::default()).is_ok()
}

/// The `check` of starboards.
pub fn check_starboard(data: &Map<String, Value>, ctx: &CheckCtx) -> Option<Problem> {
    let m = update_merged(data, ctx);
    if truthy(data.get("emojis")) {
        let list = as_list(data.get("emojis"));
        if let Some(bad) = list.iter().position(|e| !is_emoji_entry(e)) {
            let text = js_string(list.get(bad));
            return Some(Problem::new(format!("emojis.{bad}"), format!("\"{}\" is not an emoji.", slice16(&text, 40))));
        }
    }
    if truthy(m.get("selfStar")) && truthy(m.get("removeSelfStars")) {
        return Some(Problem::new("removeSelfStars", "Self-stars are counted on this board, so they cannot also be removed."));
    }
    None
}

// ── tempVoiceHubs.js ──────────────────────────────────────────────────────────────────────────────────────────

pub const DEFAULT_NAME_FORMAT: &str = "#{index} - {username}'s Channel";
pub const MAX_HUB_ROLE_LIST: usize = 10;

const JS_SPACE: &str = r"[\t\n\x0B\x0C\r \x{a0}\x{1680}\x{2000}-\x{200a}\x{2028}\x{2029}\x{202f}\x{205f}\x{3000}\x{feff}]";

/// `MENTION_TOKEN_RE` (`/\{\s*user(?:\.mention)?\s*\}|<[@#][!&]?\d+>/i`).
static MENTION_TOKEN_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(&format!(
        r"\{{{JS_SPACE}*[Uu][Ss][Ee][Rr](?:\.[Mm][Ee][Nn][Tt][Ii][Oo][Nn])?{JS_SPACE}*\}}|<[@#][!&]?[0-9]+>"
    ))
    .expect("valid regex")
});

/// The `check` of tempVoiceHubs: a channel name cannot hold a mention.
pub fn check_temp_voice_hub(data: &Map<String, Value>, _ctx: &CheckCtx) -> Option<Problem> {
    if let Some(v) = data.get("nameFormat")
        && MENTION_TOKEN_RE.is_match(&js_string(Some(v)))
    {
        return Some(Problem::new("nameFormat", "Channel names can't contain mentions. Use {username} or {user.name} instead of {user}."));
    }
    None
}

// ── statChannels.js ───────────────────────────────────────────────────────────────────────────────────────────

/// `KIND_INFO`: the counter types (the Prisma enum StatKind, same order) with their label and default name template.
pub const KIND_INFO: [(&str, &str, &str); 11] = [
    ("MEMBERS", "Members", "Members: {count}"),
    ("HUMANS", "Humans", "Humans: {count}"),
    ("BOTS", "Bots", "Bots: {count}"),
    ("ONLINE", "Online", "Online: {count}"),
    ("PEAK_ONLINE", "Peak online", "Peak Online: {count}"),
    ("TEXT_CHANNELS", "Text channels", "Text Channels: {count}"),
    ("VOICE_CHANNELS", "Voice channels", "Voice Channels: {count}"),
    ("TOTAL_CHANNELS", "All channels", "Channels: {count}"),
    ("ROLES", "Roles", "Roles: {count}"),
    ("ROLE_MEMBERS", "Role members", "{role}: {count}"),
    ("BOOSTS", "Boosts", "Boosts: {count}"),
];

pub const MAX_NAME_LENGTH: usize = 100;

/// `KIND_INFO[kind]` as (label, template).
pub fn kind_info(kind: &str) -> Option<(&'static str, &'static str)> {
    KIND_INFO.iter().find(|(k, _, _)| *k == kind).map(|(_, l, t)| (*l, *t))
}

/// `normalizeTemplate(str)` of statChannels.js: collapses white space like statsService.
pub fn normalize_stat_template(s: Option<&Value>) -> String {
    let text = match s {
        None | Some(Value::Null) => String::new(),
        Some(v) => js_string(Some(v)),
    };
    let mut out = String::with_capacity(text.len());
    let mut in_space = false;
    for c in text.chars() {
        if bot_util::js::is_js_space(c) {
            if !in_space {
                out.push(' ');
            }
            in_space = true;
        } else {
            out.push(c);
            in_space = false;
        }
    }
    bot_util::js::trim(&out).to_string()
}

/// The `check` of statChannels.
pub fn check_stat_channel(data: &Map<String, Value>, ctx: &CheckCtx) -> Option<Problem> {
    let kind = match data.get("kind").filter(|v| !v.is_null()) {
        Some(k) => Some(k),
        None => ctx.current.and_then(|c| c.get("kind")),
    };
    let role_members = kind.and_then(Value::as_str) == Some("ROLE_MEMBERS");
    if ctx.mode == Mode::Create {
        if role_members && !truthy(data.get("roleId")) {
            return Some(Problem::new("roleId", "Pick the role to count for \"Role members\"."));
        }
        if !role_members && truthy(data.get("roleId")) {
            return Some(Problem::new("roleId", "The role only applies to the \"Role members\" counter."));
        }
    }
    if let Some(Value::String(_)) = data.get("template") {
        let t = normalize_stat_template(data.get("template")).to_ascii_lowercase();
        if !t.contains("{count}") {
            return Some(Problem::new("template", "The name must contain {count}, e.g. \"Members: {count}\"."));
        }
        if t.contains("{random") {
            return Some(Problem::new("template", "Random variables are not allowed: the name would change on every refresh."));
        }
        if t.contains("{role}") && !role_members {
            return Some(Problem::new("template", "{role} only works with the \"Role members\" counter."));
        }
    }
    None
}

// ── translateChannels.js ──────────────────────────────────────────────────────────────────────────────────────

/// /autotranslate takes up to five target languages per channel.
pub const MAX_TARGETS: usize = 5;

/// `TRANSLATE_LANGUAGES`: code -> English name, in the JS order (read from the `targetLangs` options, whose labels
/// are `${name} (${code})`).
pub fn translate_languages() -> &'static [(String, String)] {
    static TABLE: LazyLock<Vec<(String, String)>> = LazyLock::new(|| {
        let Some(c) = crate::Collections::builtin().get("translateChannels") else { return Vec::new() };
        let Some(f) = c.fields.iter().find(|f| f.field() == "targetLangs") else { return Vec::new() };
        f.get("options")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|o| {
                let code = o.get("value")?.as_str()?;
                let label = o.get("label")?.as_str()?;
                let name = label.strip_suffix(&format!(" ({code})"))?;
                Some((code.to_string(), name.to_string()))
            })
            .collect()
    });
    &TABLE
}

/// `TRANSLATE_LANGUAGES[code]`.
pub fn language_name(code: &str) -> Option<&'static str> {
    translate_languages().iter().find(|(c, _)| c == code).map(|(_, n)| n.as_str())
}

/// `describeLanguages(codes)`: "Spanish, Japanese" for a list of codes.
pub fn describe_languages(codes: &[Value]) -> String {
    codes
        .iter()
        .map(|c| match c.as_str().and_then(language_name) {
            Some(n) => n.to_string(),
            None if c.is_null() => String::new(),
            None => js_string(Some(c)),
        })
        .collect::<Vec<_>>()
        .join(", ")
}
