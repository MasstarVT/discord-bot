//! Port of `src/utils/timezones.js`: time zone names (validation, search and display), member zone input, timezone
//! roles and their templates. Shared by /settings timezone, /settings timezone-role, /timezone and /timestamp.

use crate::js;
use crate::jsre::LazyRe;
use crate::lazy_re;
use crate::time::{self, format_time_of_day, is_valid_time_zone, zone_from_abbreviation, zone_from_offset, zoned_parts_ms, TimeOfDay};
use crate::tzdata;
use serde_json::Value;

/// ICU still lists legacy IANA names; these map them to the current tzdata names (JS object order).
pub const LEGACY_TZ_NAMES: &[(&str, &str)] = &[
    ("Africa/Asmera", "Africa/Asmara"),
    ("America/Buenos_Aires", "America/Argentina/Buenos_Aires"),
    ("America/Catamarca", "America/Argentina/Catamarca"),
    ("America/Cordoba", "America/Argentina/Cordoba"),
    ("America/Jujuy", "America/Argentina/Jujuy"),
    ("America/Mendoza", "America/Argentina/Mendoza"),
    ("America/Coral_Harbour", "America/Atikokan"),
    ("America/Godthab", "America/Nuuk"),
    ("America/Indianapolis", "America/Indiana/Indianapolis"),
    ("America/Louisville", "America/Kentucky/Louisville"),
    ("Asia/Calcutta", "Asia/Kolkata"),
    ("Asia/Katmandu", "Asia/Kathmandu"),
    ("Asia/Rangoon", "Asia/Yangon"),
    ("Asia/Saigon", "Asia/Ho_Chi_Minh"),
    ("Atlantic/Faeroe", "Atlantic/Faroe"),
    ("Europe/Kiev", "Europe/Kyiv"),
    ("Pacific/Enderbury", "Pacific/Kanton"),
    ("Pacific/Ponape", "Pacific/Pohnpei"),
    ("Pacific/Truk", "Pacific/Chuuk"),
];

fn legacy(name: &str) -> Option<&'static str> {
    LEGACY_TZ_NAMES.iter().find(|(k, _)| *k == name).map(|(_, v)| *v)
}

const UTC_ALIASES: [&str; 18] = [
    "utc", "gmt", "uct", "zulu", "universal", "greenwich", "etc/utc", "etc/gmt", "etc/uct", "etc/zulu", "etc/universal", "etc/greenwich",
    "gmt0", "gmt+0", "gmt-0", "etc/gmt0", "etc/gmt+0", "etc/gmt-0",
];

/// Shown when the zone box is empty (after the current zone), in this order.
pub const POPULAR_TIMEZONES: [&str; 25] = [
    "UTC", "America/New_York", "America/Chicago", "America/Denver", "America/Los_Angeles", "America/Toronto", "America/Mexico_City",
    "America/Sao_Paulo", "Europe/London", "Europe/Paris", "Europe/Berlin", "Europe/Madrid", "Europe/Istanbul", "Europe/Moscow",
    "Africa/Cairo", "Africa/Johannesburg", "Asia/Dubai", "Asia/Kolkata", "Asia/Bangkok", "Asia/Singapore", "Asia/Shanghai",
    "Asia/Tokyo", "Asia/Seoul", "Australia/Sydney", "Pacific/Auckland",
];

/// Every zone the autocomplete offers: 'UTC' first, then the ICU zones (legacy names mapped), sorted.
pub fn list_time_zones() -> &'static [String] {
    tzdata::zone_list()
}

/// Why a typed zone was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZoneErrorCode {
    Empty,
    Unknown,
    Offset,
    Abbreviation,
    Ambiguous,
}

impl ZoneErrorCode {
    /// 'EMPTY', 'UNKNOWN', 'OFFSET', 'ABBREVIATION' or 'AMBIGUOUS'.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Empty => "EMPTY",
            Self::Unknown => "UNKNOWN",
            Self::Offset => "OFFSET",
            Self::Abbreviation => "ABBREVIATION",
            Self::Ambiguous => "AMBIGUOUS",
        }
    }
}

static OFFSET_LEAD: LazyRe = lazy_re!(r"^[+\-−\d]", "");
static UTC_OFFSET_LEAD: LazyRe = lazy_re!(r"^(?:utc|gmt)\s*[+\-−]\s*\d", "i");

/// Validates and canonicalises a time zone typed by an admin: `Ok(zone)` or the reason it was refused.
pub fn normalize_time_zone(input: &str) -> Result<String, ZoneErrorCode> {
    let s = js::trim(input);
    if s.is_empty() {
        return Err(ZoneErrorCode::Empty);
    }
    if js::utf16_len(s) > 64 {
        return Err(ZoneErrorCode::Unknown);
    }
    if UTC_ALIASES.contains(&s.to_lowercase().as_str()) {
        return Ok("UTC".into());
    }
    if OFFSET_LEAD.test(s) || UTC_OFFSET_LEAD.test(s) {
        return Err(ZoneErrorCode::Offset);
    }
    if !is_valid_time_zone(s) {
        return Err(ZoneErrorCode::Unknown);
    }
    if !s.contains('/') {
        return Err(ZoneErrorCode::Abbreviation);
    }
    let lower = s.to_lowercase();
    if let Some(hit) = list_time_zones().iter().find(|z| z.to_lowercase() == lower) {
        return Ok(hit.clone());
    }
    let canonical = time::canonical_zone(s).unwrap_or_else(|| s.to_string());
    if UTC_ALIASES.contains(&canonical.to_lowercase().as_str()) {
        return Ok("UTC".into());
    }
    Ok(match legacy(&canonical) {
        Some(m) if is_valid_time_zone(m) => m.to_string(),
        _ => canonical,
    })
}

/// 'UTC+05:30'-style label for a zone at an instant (DST aware); 'UTC' for an unknown zone or an invalid date.
pub fn utc_offset_label_ms(zone: &str, date_ms: f64) -> String {
    let Some((_, z)) = tzdata::resolve(zone) else { return "UTC".into() };
    let t = js::time_clip(date_ms);
    if t.is_nan() {
        return "UTC".into();
    }
    let secs = tzdata::offset_seconds(z, t);
    if secs == 0 || secs % 60 != 0 {
        // ICU prints a bare 'GMT' for +00:00, and 'GMT-04:56:02' (which the JS does not read) for a seconds offset.
        return "UTC+00:00".into();
    }
    let a = secs.abs();
    format!("UTC{}{:02}:{:02}", if secs < 0 { '-' } else { '+' }, a / 3600, (a % 3600) / 60)
}

/// [`utc_offset_label_ms`] at a `DateTime`.
pub fn utc_offset_label(zone: &str, date: &chrono::DateTime<chrono::Utc>) -> String {
    utc_offset_label_ms(zone, time::to_ms(date))
}

/// One autocomplete choice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZoneChoice {
    pub name: String,
    pub value: String,
}

/// Options of [`search_time_zones`].
#[derive(Debug, Clone)]
pub struct SearchOptions<'a> {
    pub current: Option<&'a str>,
    pub now_ms: f64,
    pub limit: i64,
    pub abbreviations: bool,
}

impl Default for SearchOptions<'_> {
    fn default() -> Self {
        Self { current: None, now_ms: js::now_ms(), limit: 25, abbreviations: false }
    }
}

static WS: LazyRe = lazy_re!(r"\s+", "");
static OFF_QUERY: LazyRe = lazy_re!(r"^(?:utc|gmt)?_?([+\-−])_?(\d{1,2})(?::?(\d{2}))?$", "");

/// Autocomplete choices for a zone option: by name or city, or by offset ('+5:30', 'utc-3'). With `abbreviations`, an
/// abbreviation such as 'cst' puts the zone it names first.
pub fn search_time_zones(query: &str, o: &SearchOptions<'_>) -> Vec<ZoneChoice> {
    let list = list_time_zones();
    let known = |z: &str| list.iter().any(|x| x == z);
    let q = WS.replace_all(&js::trim(query).to_lowercase(), "_");
    let mut zones: Vec<String>;
    if q.is_empty() {
        zones = Vec::new();
        if let Some(cur) = o.current
            && known(cur) {
                zones.push(cur.to_string());
            }
        zones.extend(POPULAR_TIMEZONES.iter().filter(|z| known(z) && Some(**z) != o.current).map(|z| z.to_string()));
    } else if let Some((t, m)) = OFF_QUERY.exec(&q) {
        let hh: u32 = m.group(&t, 2).and_then(|x| x.parse().ok()).unwrap_or(0);
        if hh > 14 {
            return Vec::new();
        }
        let tail = format!("{hh:02}{}", m.group(&t, 3).map(|x| format!(":{x}")).unwrap_or_default());
        let sign = if m.group(&t, 1).as_deref() == Some("+") { '+' } else { '-' };
        let prefix = format!("UTC{sign}{tail}");
        let zero_prefix = format!("UTC+{tail}");
        let mut labelled: Vec<(String, String)> = list
            .iter()
            .map(|z| (z.clone(), utc_offset_label_ms(z, o.now_ms)))
            .filter(|(_, l)| l.starts_with(&prefix) || (hh == 0 && l.starts_with(&zero_prefix)))
            .collect();
        labelled.sort_by(|a, b| tzdata::locale_compare(&a.1, &b.1).then_with(|| tzdata::locale_compare(&a.0, &b.0)));
        zones = labelled.into_iter().map(|(z, _)| z).collect();
    } else {
        let mut scored: Vec<(u8, &String)> = Vec::new();
        for z in list {
            let zl = z.to_lowercase();
            let city = &zl[zl.rfind('/').map_or(0, |i| i + 1)..];
            let score = if zl == q || city == q {
                0
            } else if city.starts_with(&q) {
                1
            } else if zl.starts_with(&q) {
                2
            } else if zl.contains(&q) {
                3
            } else {
                continue;
            };
            scored.push((score, z));
        }
        scored.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| tzdata::locale_compare(a.1, b.1)));
        zones = scored.into_iter().map(|(_, z)| z.clone()).collect();
    }
    let abbr = if o.abbreviations { zone_from_abbreviation(query) } else { None };
    if let Some(a) = abbr {
        zones.retain(|z| z != a);
        zones.insert(0, a.to_string());
    }
    let n = zones.len() as i64;
    let end = if o.limit < 0 { (n + o.limit).max(0) } else { o.limit.min(n) } as usize;
    zones
        .into_iter()
        .take(end)
        .map(|z| {
            let mut name = format!("{z} ({})", utc_offset_label_ms(&z, o.now_ms));
            if abbr == Some(z.as_str()) && z != "UTC" {
                name.push_str(&format!(" · {}", js::trim(query).to_uppercase()));
            }
            if o.current == Some(z.as_str()) {
                name.push_str(" · current");
            }
            ZoneChoice { name, value: z }
        })
        .collect()
}

const WEEKDAY_LONG: [&str; 7] = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];

/// Local time in a zone, for replies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalTimeInfo {
    /// False when the zone was invalid and UTC was used instead.
    pub valid: bool,
    pub zone: String,
    /// 'HH:MM'
    pub time: String,
    /// 'YYYY-MM-DD'
    pub date: String,
    pub weekday: String,
    pub offset: String,
}

/// Local time in a zone; an invalid zone falls back to UTC (`valid` is then false).
pub fn describe_local_time_ms(zone: &str, now_ms: f64) -> LocalTimeInfo {
    let tz = if is_valid_time_zone(zone) { zone } else { "UTC" };
    let p = zoned_parts_ms(now_ms, tz);
    LocalTimeInfo {
        valid: tz == zone,
        zone: tz.to_string(),
        time: format_time_of_day(TimeOfDay { hour: p.hour, minute: p.minute }),
        date: format!("{}-{:02}-{:02}", p.year, p.month, p.day),
        weekday: WEEKDAY_LONG[p.weekday as usize % 7].to_string(),
        offset: utc_offset_label_ms(tz, now_ms),
    }
}

/// A zone a member typed, accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedZone {
    pub zone: String,
    /// The abbreviation typed (upper-cased), when the zone came from one.
    pub abbreviation: Option<String>,
}

/// A zone a member typed, refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZoneInputError {
    pub code: ZoneErrorCode,
    /// For AMBIGUOUS: the zones to suggest.
    pub options: Option<Vec<String>>,
}

fn ambiguous(lower: &str) -> Option<&'static [&'static str]> {
    match lower {
        "ist" => Some(&["Asia/Kolkata", "Europe/Dublin", "Asia/Jerusalem"]),
        "ast" => Some(&["America/Halifax", "America/Puerto_Rico", "Asia/Riyadh"]),
        _ => None,
    }
}

/// A time zone a member typed: a Region/City zone or 'UTC', or a common abbreviation such as CST (read as the region
/// it names, with daylight saving). Fixed offsets are refused.
pub fn parse_zone_input(input: &str) -> Result<ParsedZone, ZoneInputError> {
    let s = js::trim(input);
    if let Some(named) = zone_from_abbreviation(s) {
        return Ok(ParsedZone { zone: named.to_string(), abbreviation: if named == "UTC" { None } else { Some(s.to_uppercase()) } });
    }
    if let Some(opts) = ambiguous(&s.to_lowercase()) {
        return Err(ZoneInputError { code: ZoneErrorCode::Ambiguous, options: Some(opts.iter().map(|z| z.to_string()).collect()) });
    }
    normalize_time_zone(s).map(|zone| ParsedZone { zone, abbreviation: None }).map_err(|code| ZoneInputError { code, options: None })
}

static ROLE_TOKEN: LazyRe = lazy_re!(r"(?:utc|gmt)\s*[+\-−]\s*\d{1,2}(?::?00)?(?![\d:])|[a-z]+", "");

/// The zone a timezone role's name names: an abbreviation ('CST', 'CST/CDT', '🇺🇸 Eastern (EST)') or a whole-hour
/// offset ('UTC-6'). Abbreviations beat offsets. None when it names no zone, or several different ones.
pub fn zone_from_role_name(name: &str) -> Option<String> {
    let lower = name.to_lowercase();
    let t = crate::jsre::u16s(&lower);
    let mut named: Vec<String> = Vec::new();
    let mut offsets: Vec<String> = Vec::new();
    for m in ROLE_TOKEN.find_all(&t) {
        let tok = m.text(&t);
        if let Some(a) = zone_from_abbreviation(&tok) {
            if !named.iter().any(|x| x == a) {
                named.push(a.to_string());
            }
        } else if let Some(off) = zone_from_offset(&tok)
            && !offsets.contains(&off) {
                offsets.push(off);
            }
    }
    if named.len() > 1 {
        named.retain(|z| z != "UTC");
    }
    if named.len() == 1 {
        return named.pop();
    }
    if named.is_empty() && offsets.len() == 1 {
        return offsets.pop();
    }
    None
}

/// One timezone role link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimezoneRole {
    pub role_id: String,
    pub zone: String,
}

fn is_snowflake(s: &str) -> bool {
    (17..=20).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_digit())
}

/// `GuildSettings.timezoneRoles` (JSON) as a clean list: valid zones, each role once, in stored order.
pub fn normalize_timezone_roles(raw: &Value) -> Vec<TimezoneRole> {
    let Some(arr) = raw.as_array() else { return Vec::new() };
    let mut out: Vec<TimezoneRole> = Vec::new();
    for r in arr {
        let Some(role_id) = r.get("roleId").and_then(Value::as_str) else { continue };
        if !is_snowflake(role_id) || out.iter().any(|x| x.role_id == role_id) {
            continue;
        }
        let Some(zone) = r.get("zone").and_then(Value::as_str) else { continue };
        if !is_valid_time_zone(zone) {
            continue;
        }
        out.push(TimezoneRole { role_id: role_id.to_string(), zone: zone.to_string() });
    }
    out
}

/// Where a member's zone came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZoneSource {
    Personal,
    Role,
    Server,
    Default,
}

impl ZoneSource {
    /// 'personal', 'role', 'server' or 'default'.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Personal => "personal",
            Self::Role => "role",
            Self::Server => "server",
            Self::Default => "default",
        }
    }
}

/// The zone a member's times are read in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedTimezone {
    pub zone: String,
    pub source: ZoneSource,
    /// The winning timezone role the member holds (also when their own zone beats it).
    pub role_id: Option<String>,
    pub role_count: usize,
}

/// The zone a member's times are read in: their own zone, else their timezone role (the highest-positioned one),
/// else the server time zone, else UTC. `settings` is `(GuildSettings.timezone, GuildSettings.timezoneRoles)`.
pub fn resolve_timezone(
    personal: Option<&str>,
    member_role_ids: &[String],
    role_position: impl Fn(&str) -> i64,
    settings: Option<(Option<&str>, &Value)>,
) -> ResolvedTimezone {
    let links: Vec<TimezoneRole> = settings
        .map(|(_, roles)| normalize_timezone_roles(roles))
        .unwrap_or_default()
        .into_iter()
        .filter(|l| member_role_ids.contains(&l.role_id))
        .collect();
    let mut best: Option<&TimezoneRole> = None;
    for link in &links {
        if best.is_none_or(|b| role_position(&link.role_id) > role_position(&b.role_id)) {
            best = Some(link);
        }
    }
    let role_id = best.map(|b| b.role_id.clone());
    let role_count = links.len();
    if let Some(p) = personal.filter(|p| is_valid_time_zone(p)) {
        return ResolvedTimezone { zone: p.to_string(), source: ZoneSource::Personal, role_id, role_count };
    }
    if let Some(b) = best {
        return ResolvedTimezone { zone: b.zone.clone(), source: ZoneSource::Role, role_id, role_count };
    }
    if let Some(tz) = settings.and_then(|(tz, _)| tz).filter(|t| is_valid_time_zone(t)) {
        return ResolvedTimezone { zone: tz.to_string(), source: ZoneSource::Server, role_id, role_count };
    }
    ResolvedTimezone { zone: "UTC".into(), source: ZoneSource::Default, role_id, role_count }
}

const MONTH_SHORT: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
const WEEKDAY_SHORT: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

/// An instant as wall-clock text in a zone: 'Fri, Oct 16, 2026, 7:00 PM' (ICU en-US). An invalid zone is UTC; an
/// invalid date gives '' (the JS throws a RangeError there).
pub fn format_in_zone_ms(date_ms: f64, zone: &str) -> String {
    let tz = if is_valid_time_zone(zone) { zone } else { "UTC" };
    let t = js::time_clip(date_ms);
    if t.is_nan() {
        return String::new();
    }
    let p = zoned_parts_ms(t, tz);
    // The weekday of the local date itself (the era year of zoned_parts would mislead for BC dates).
    let z = tzdata::resolve(tz).map(|(_, z)| z).unwrap_or(tzdata::Zone::Fixed(0));
    let local = js::utc_fields(t + tzdata::offset_seconds(z, t) as f64 * 1000.0);
    let (h12, ap) = match p.hour {
        0 => (12, "AM"),
        1..=11 => (p.hour, "AM"),
        12 => (12, "PM"),
        h => (h - 12, "PM"),
    };
    format!(
        "{}, {} {}, {}, {}:{:02} {}",
        WEEKDAY_SHORT[local.weekday as usize],
        MONTH_SHORT[(p.month as usize).saturating_sub(1) % 12],
        p.day,
        p.year,
        h12,
        p.minute,
        ap
    )
}

/// [`format_in_zone_ms`] for a `DateTime`.
pub fn format_in_zone(date: &chrono::DateTime<chrono::Utc>, zone: &str) -> String {
    format_in_zone_ms(time::to_ms(date), zone)
}

static ETC_GMT: LazyRe = lazy_re!(r"^Etc\/GMT[+-]\d{1,2}$", "");

/// A zone for replies: 'America/Chicago (UTC-05:00)', 'UTC', or 'UTC+02:00' for an Etc/GMT offset zone.
pub fn zone_label_ms(zone: &str, date_ms: f64) -> String {
    if zone == "UTC" {
        return "UTC".into();
    }
    if ETC_GMT.test(zone) {
        return utc_offset_label_ms(zone, date_ms);
    }
    format!("{zone} ({})", utc_offset_label_ms(zone, date_ms))
}

/// [`zone_label_ms`] for a `DateTime`.
pub fn zone_label(zone: &str, date: &chrono::DateTime<chrono::Utc>) -> String {
    zone_label_ms(zone, time::to_ms(date))
}

/// Where a member's zone came from, for replies (`source` is 'personal', 'role', 'server' or anything else).
pub fn zone_source_text(source: &str, role_id: Option<&str>, zone_from_input: bool) -> String {
    if zone_from_input {
        return "the timezone in your text".into();
    }
    match source {
        "personal" => "your saved timezone (`/timezone set`)".into(),
        "role" => format!("your <@&{}> role", role_id.unwrap_or("null")),
        "server" => "this server's timezone".into(),
        _ => "UTC, because no timezone is set".into(),
    }
}

fn shown_zone(raw: &str) -> String {
    let cleaned: String = raw.chars().filter(|c| *c != '`' && *c != '\\').collect();
    let s = js::utf16_prefix(js::trim(&cleaned), 64);
    if s.is_empty() { "(empty)".into() } else { s.to_string() }
}

/// Title and text for a [`parse_zone_input`] failure (`code` as its string; `options` for AMBIGUOUS).
pub fn zone_input_error(raw: &str, code: &str, options: &[String]) -> (String, String) {
    match code {
        "EMPTY" => (
            "Missing Timezone".into(),
            "Pick a timezone from the list, e.g. `America/Chicago`, or type an abbreviation like `CST`.".into(),
        ),
        "OFFSET" => (
            "Pick a City".into(),
            "Fixed offsets like `-06:00` ignore daylight saving time. Type the offset in the **zone** box to see the cities that use it, then pick one.".into(),
        ),
        "AMBIGUOUS" => (
            "Ambiguous Timezone".into(),
            format!(
                "`{}` can mean several timezones: {}. Pick the one you mean.",
                shown_zone(raw),
                options.iter().map(|z| format!("`{z}`")).collect::<Vec<_>>().join(", ")
            ),
        ),
        "ABBREVIATION" => (
            "Pick a City".into(),
            format!(
                "`{}` is not a name I can be sure of. Pick a Region/City zone from the list, e.g. `America/New_York` or `Europe/Paris`.",
                shown_zone(raw)
            ),
        ),
        _ => (
            "Unknown Timezone".into(),
            format!(
                "`{}` is not a timezone I know. Pick one from the list, e.g. `America/Chicago`, `Europe/London` or `Asia/Tokyo`, or type an abbreviation like `CST`, `EST` or `PST`.",
                shown_zone(raw)
            ),
        ),
    }
}

// ── Timezone role templates ────────────────────────────────────────────────────

/// One role of a timezone panel template.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TzRoleTemplate {
    /// The Discord role name.
    pub name: &'static str,
    pub zone: &'static str,
    /// The panel option label (≤ 80).
    pub label: &'static str,
    /// The panel option description (≤ 100).
    pub description: &'static str,
}

const fn tz_role(name: &'static str, zone: &'static str, label: &'static str, description: &'static str) -> TzRoleTemplate {
    TzRoleTemplate { name, zone, label, description }
}

const HST: TzRoleTemplate = tz_role("HST", "Pacific/Honolulu", "Hawaii (HST)", "Honolulu");
const AKST: TzRoleTemplate = tz_role("AKST", "America/Anchorage", "Alaska (AKST/AKDT)", "Anchorage, Juneau");
const PST: TzRoleTemplate = tz_role("PST", "America/Los_Angeles", "Pacific (PST/PDT)", "Los Angeles, Seattle, Vancouver");
const MST_AZ: TzRoleTemplate = tz_role("MST (AZ)", "America/Phoenix", "Arizona (MST, no daylight saving)", "Phoenix, Tucson");
const MST: TzRoleTemplate = tz_role("MST", "America/Denver", "Mountain (MST/MDT)", "Denver, Salt Lake City, Calgary");
const CST: TzRoleTemplate = tz_role("CST", "America/Chicago", "Central (CST/CDT)", "Chicago, Dallas, Winnipeg");
const EST: TzRoleTemplate = tz_role("EST", "America/New_York", "Eastern (EST/EDT)", "New York, Toronto, Miami");
const AST_PR: TzRoleTemplate =
    tz_role("AST (PR)", "America/Puerto_Rico", "Atlantic (AST, no daylight saving)", "Puerto Rico, U.S. Virgin Islands, Santo Domingo");
const CHST: TzRoleTemplate = tz_role("ChST", "Pacific/Guam", "Chamorro (ChST)", "Guam, Northern Mariana Islands");

/// A timezone panel template.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimezoneRoleTemplate {
    pub key: &'static str,
    pub label: &'static str,
    pub roles: &'static [TzRoleTemplate],
}

/// `TIMEZONE_ROLE_TEMPLATES`, in the JS order.
pub const TIMEZONE_ROLE_TEMPLATES: [TimezoneRoleTemplate; 2] = [
    TimezoneRoleTemplate {
        key: "timezones-us",
        label: "Timezones: US",
        roles: &[tz_role("SST", "Pacific/Pago_Pago", "Samoa (SST)", "American Samoa"), HST, AKST, PST, MST_AZ, MST, CST, EST, AST_PR, CHST],
    },
    TimezoneRoleTemplate {
        key: "timezones-world",
        label: "Timezones: worldwide",
        roles: &[
            HST,
            AKST,
            PST,
            MST_AZ,
            MST,
            tz_role("CST (MX)", "America/Mexico_City", "Mexico (CST, no daylight saving)", "Mexico City, Guadalajara, Monterrey"),
            CST,
            EST,
            AST_PR,
            tz_role("AST", "America/Halifax", "Atlantic (AST/ADT)", "Halifax, Bermuda"),
            tz_role("BRT", "America/Sao_Paulo", "Brasília (BRT)", "São Paulo, Rio de Janeiro"),
            tz_role("GMT", "Europe/London", "UK & Ireland (GMT/BST)", "London, Dublin, Lisbon"),
            tz_role("CET", "Europe/Berlin", "Central Europe (CET/CEST)", "Berlin, Paris, Rome, Madrid"),
            tz_role("EET", "Europe/Athens", "Eastern Europe (EET/EEST)", "Athens, Helsinki, Kyiv"),
            tz_role("MSK", "Europe/Moscow", "Moscow (MSK)", "Moscow, Istanbul"),
            tz_role("GST", "Asia/Dubai", "Gulf (GST)", "Dubai, Abu Dhabi"),
            tz_role("PKT", "Asia/Karachi", "Pakistan (PKT)", "Karachi, Lahore, Islamabad"),
            tz_role("IST", "Asia/Kolkata", "India (IST)", "Mumbai, Delhi, Bengaluru"),
            tz_role("ICT", "Asia/Bangkok", "Indochina (ICT)", "Bangkok, Jakarta, Hanoi"),
            tz_role("SGT", "Asia/Singapore", "Singapore & China (UTC+8)", "Singapore, Beijing, Manila, Perth"),
            tz_role("JST", "Asia/Tokyo", "Japan & Korea (JST/KST)", "Tokyo, Seoul"),
            CHST,
            tz_role("AEST", "Australia/Sydney", "Sydney (AEST/AEDT)", "Sydney, Melbourne"),
            tz_role("NZST", "Pacific/Auckland", "New Zealand (NZST/NZDT)", "Auckland, Wellington"),
        ],
    },
];

/// The template with this key.
pub fn timezone_role_template(key: &str) -> Option<&'static TimezoneRoleTemplate> {
    TIMEZONE_ROLE_TEMPLATES.iter().find(|t| t.key == key)
}

/// `TIMEZONE_TEMPLATE_PAST_ZONES`: the zones of each template's panel before the template gained roles.
pub const TIMEZONE_TEMPLATE_PAST_ZONES: [(&str, &[&[&str]]); 2] = [
    (
        "timezones-us",
        &[&["Pacific/Honolulu", "America/Anchorage", "America/Los_Angeles", "America/Denver", "America/Chicago", "America/New_York"]],
    ),
    (
        "timezones-world",
        &[&[
            "Pacific/Honolulu",
            "America/Anchorage",
            "America/Los_Angeles",
            "America/Denver",
            "America/Chicago",
            "America/New_York",
            "America/Halifax",
            "America/Sao_Paulo",
            "Europe/London",
            "Europe/Berlin",
            "Europe/Athens",
            "Europe/Moscow",
            "Asia/Dubai",
            "Asia/Kolkata",
            "Asia/Bangkok",
            "Asia/Singapore",
            "Asia/Tokyo",
            "Australia/Sydney",
            "Pacific/Auckland",
        ]],
    ),
];

/// The text of a timezone panel (`TIMEZONE_PANEL`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimezonePanel {
    pub kind: &'static str,
    pub layout: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub placeholder: &'static str,
}

/// `TIMEZONE_PANEL`: Unique mode (one role at a time) and a dropdown.
pub const TIMEZONE_PANEL: TimezonePanel = TimezonePanel {
    kind: "UNIQUE",
    layout: "SELECT",
    title: "🌍 Timezone Roles",
    description: "Pick your timezone so `/timestamp` reads your times right. You can hold one timezone role at a time; pick another to switch.",
    placeholder: "Pick your timezone",
};
