//! Port of `src/services/translationService.js` (pkg-translation-fixes): the language registry and code
//! normalisation, the Google (keyless) and LibreTranslate adapters, a circuit breaker, request sizing, a short-text
//! result cache, the channel-configuration cache and the auto-translate engine used by the `translation-fixes`
//! messageCreate hook. Nothing here fails into a caller except [`translate_text`], [`detect_language`] and
//! [`get_guild_translate_config`], which return a [`TranslationError`] / the database error.
//!
//! The JS module variables (pause, LibreTranslate support, failover, channel memo, throttles) live in the app's
//! [`TranslationState`] (`app.ext`). Every HTTP request goes through an [`HttpTransport`] (reqwest by default), which
//! tests replace with canned responses ([`set_transport`]); [`configure`] and [`reset`] are the JS `__testing` seam.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};

use bot_core::App;
use bot_db::{DbError, GuildSettings, TRANSLATE_CHANNEL_INFO, TranslateChannel};
use bot_util::js;
use bot_util::jsre::LazyRe;
use bot_util::lazy_re;
use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
use indexmap::IndexMap;
use serde_json::{Value, json};
use serenity::builder::{CreateAllowedMentions, CreateEmbed, CreateEmbedFooter, CreateMessage};

use crate::translate_stats_service::{self as stats, StatVia};

// ── Constants ─────────────────────────────────────────────────────────────────

pub const GOOGLE_ENDPOINT: &str = "https://translate.googleapis.com/translate_a/single";
/// Only the `gtx` client is used. A 429 trips the circuit breaker (pause) instead of switching to another client.
pub const GOOGLE_CLIENTS: [&str; 1] = ["gtx"];
pub const GOOGLE_CLIENT_BLOCK_MS: i64 = 10 * 60_000;
pub const GOOGLE_TIMEOUT_MS: u64 = 8_000;
pub const LT_TIMEOUT_MS: u64 = 15_000;
pub const LT_LANGUAGES_TIMEOUT_MS: u64 = 5_000;
/// Re-fetch the LibreTranslate language list every 6 h.
pub const LT_LANGUAGES_TTL_MS: i64 = 6 * 60 * 60_000;
/// After a failed language-list fetch.
pub const LT_LANGUAGES_RETRY_MS: i64 = 5 * 60_000;
/// After the main LibreTranslate fails, use the fallback this long.
pub const LT_FAILOVER_MS: i64 = 5 * 60_000;
const LT_FAILOVER_STATUS: [u16; 3] = [502, 503, 504];
/// `PAUSE_MS.google`.
pub const PAUSE_MS_GOOGLE: f64 = 30.0 * 60_000.0;
/// `PAUSE_MS.libretranslate`.
pub const PAUSE_MS_LIBRETRANSLATE: f64 = 60_000.0;
/// Cap for Retry-After.
pub const PAUSE_MAX_MS: f64 = 6.0 * 60.0 * 60_000.0;
/// Google GET: q stays <= 6,000 URL-encoded bytes.
pub const MAX_Q_ENCODED_BYTES: usize = 6_000;
/// Above the GET limit, one POST (form body) up to this size.
pub const MAX_POST_Q_BYTES: usize = 24_000;
pub const AUTO_INPUT_MAX_CODEPOINTS: usize = 1_000;
pub const DETECT_SAMPLE_CODEPOINTS: usize = 500;
/// Only used above MAX_POST_Q_BYTES, which /translate (1,500 code points) never reaches.
pub const TRANSLATE_MAX_CHUNKS: usize = 4;
pub const AUTO_PER_TARGET_MAX: i64 = 1_000;
pub const AUTO_DESCRIPTION_BUDGET: i64 = 4_000;
pub const AUTO_DEADLINE_MS: i64 = 25_000;
pub const AUTO_MAX_INFLIGHT: usize = 6;
pub const RESULT_CACHE_TTL_S: u64 = 3_600;
pub const RESULT_CACHE_MAX_CODEPOINTS: usize = 300;
pub const CHANNELS_CACHE_TTL_S: u64 = 300;
pub const CHANNELS_MEMO_TTL_MS: i64 = 30_000;
pub const EMBED_PART_MAX: usize = 4_000;
pub const EMBED_MAX_PARTS: usize = 3;

/// At most this many target languages per channel.
pub const MAX_TARGETS: usize = 5;
const CHANNEL_MEMO_MAX: usize = 10_000;

// ── Language registry ─────────────────────────────────────────────────────────

/// `LANGUAGES`, in the JS key order.
pub static LANGUAGES: &[(&str, &str)] = &[
    ("af", "Afrikaans"),
    ("ar", "Arabic"),
    ("az", "Azerbaijani"),
    ("be", "Belarusian"),
    ("bg", "Bulgarian"),
    ("bn", "Bengali"),
    ("bs", "Bosnian"),
    ("ca", "Catalan"),
    ("cs", "Czech"),
    ("cy", "Welsh"),
    ("da", "Danish"),
    ("de", "German"),
    ("el", "Greek"),
    ("en", "English"),
    ("eo", "Esperanto"),
    ("es", "Spanish"),
    ("et", "Estonian"),
    ("fa", "Persian"),
    ("fi", "Finnish"),
    ("fr", "French"),
    ("ga", "Irish"),
    ("gl", "Galician"),
    ("gu", "Gujarati"),
    ("he", "Hebrew"),
    ("hi", "Hindi"),
    ("hr", "Croatian"),
    ("hu", "Hungarian"),
    ("hy", "Armenian"),
    ("id", "Indonesian"),
    ("is", "Icelandic"),
    ("it", "Italian"),
    ("ja", "Japanese"),
    ("ka", "Georgian"),
    ("kk", "Kazakh"),
    ("km", "Khmer"),
    ("ko", "Korean"),
    ("lt", "Lithuanian"),
    ("lv", "Latvian"),
    ("mk", "Macedonian"),
    ("ml", "Malayalam"),
    ("mn", "Mongolian"),
    ("mr", "Marathi"),
    ("ms", "Malay"),
    ("mt", "Maltese"),
    ("my", "Myanmar (Burmese)"),
    ("ne", "Nepali"),
    ("nl", "Dutch"),
    ("no", "Norwegian"),
    ("pa", "Punjabi"),
    ("pl", "Polish"),
    ("pt", "Portuguese"),
    ("ro", "Romanian"),
    ("ru", "Russian"),
    ("si", "Sinhala"),
    ("sk", "Slovak"),
    ("sl", "Slovenian"),
    ("sq", "Albanian"),
    ("sr", "Serbian"),
    ("sv", "Swedish"),
    ("sw", "Swahili"),
    ("ta", "Tamil"),
    ("te", "Telugu"),
    ("th", "Thai"),
    ("tl", "Filipino"),
    ("tr", "Turkish"),
    ("uk", "Ukrainian"),
    ("ur", "Urdu"),
    ("uz", "Uzbek"),
    ("vi", "Vietnamese"),
    ("xh", "Xhosa"),
    ("yi", "Yiddish"),
    ("zh", "Chinese (Simplified)"),
    ("zh-tw", "Chinese (Traditional)"),
    ("zu", "Zulu"),
];

/// Language code -> ISO 3166-1 alpha-2 country code for the flag emoji (`None`: a globe).
static LANG_FLAGS: &[(&str, Option<&str>)] = &[
    ("af", Some("ZA")), ("ar", Some("SA")), ("az", Some("AZ")), ("be", Some("BY")), ("bg", Some("BG")), ("bn", Some("BD")), ("bs", Some("BA")),
    ("ca", Some("ES")), ("cs", Some("CZ")), ("cy", Some("GB")), ("da", Some("DK")), ("de", Some("DE")), ("el", Some("GR")), ("en", Some("GB")),
    ("eo", None), ("es", Some("ES")), ("et", Some("EE")), ("fa", Some("IR")), ("fi", Some("FI")), ("fr", Some("FR")), ("ga", Some("IE")),
    ("gl", Some("ES")), ("gu", Some("IN")), ("he", Some("IL")), ("hi", Some("IN")), ("hr", Some("HR")), ("hu", Some("HU")), ("hy", Some("AM")),
    ("id", Some("ID")), ("is", Some("IS")), ("it", Some("IT")), ("ja", Some("JP")), ("ka", Some("GE")), ("kk", Some("KZ")), ("km", Some("KH")),
    ("ko", Some("KR")), ("lt", Some("LT")), ("lv", Some("LV")), ("mk", Some("MK")), ("ml", Some("IN")), ("mn", Some("MN")), ("mr", Some("IN")),
    ("ms", Some("MY")), ("mt", Some("MT")), ("my", Some("MM")), ("ne", Some("NP")), ("nl", Some("NL")), ("no", Some("NO")), ("pa", Some("IN")),
    ("pl", Some("PL")), ("pt", Some("PT")), ("ro", Some("RO")), ("ru", Some("RU")), ("si", Some("LK")), ("sk", Some("SK")), ("sl", Some("SI")),
    ("sq", Some("AL")), ("sr", Some("RS")), ("sv", Some("SE")), ("sw", Some("TZ")), ("ta", Some("IN")), ("te", Some("IN")), ("th", Some("TH")),
    ("tl", Some("PH")), ("tr", Some("TR")), ("uk", Some("UA")), ("ur", Some("PK")), ("uz", Some("UZ")), ("vi", Some("VN")), ("xh", Some("ZA")),
    ("yi", Some("IL")), ("zh", Some("CN")), ("zh-tw", Some("TW")), ("zu", Some("ZA")),
];

/// Legacy and regional codes -> canonical codes. Google reports Hebrew as `iw`; LibreTranslate says `nb`.
pub static CODE_ALIASES: &[(&str, &str)] = &[
    ("iw", "he"), ("ji", "yi"), ("jw", "jv"), ("in", "id"),
    ("zh-cn", "zh"), ("zh-sg", "zh"), ("zh-hans", "zh"),
    ("zh-hk", "zh-tw"), ("zh-mo", "zh-tw"), ("zh-hant", "zh-tw"), ("zt", "zh-tw"),
    ("nb", "no"), ("nn", "no"), ("fil", "tl"), ("pb", "pt"), ("pt-br", "pt"), ("pt-pt", "pt"),
];
static NAME_ALIASES: &[(&str, &str)] = &[
    ("chinese", "zh"), ("simplified chinese", "zh"), ("traditional chinese", "zh-tw"),
    ("burmese", "my"), ("myanmar", "my"), ("tagalog", "tl"), ("farsi", "fa"),
    ("norwegian bokmål", "no"), ("norwegian bokmal", "no"),
];
static DID_YOU_MEAN: &[(&str, &str)] = &[
    ("jp", "ja"), ("kr", "ko"), ("cn", "zh"), ("tw", "zh-tw"), ("hk", "zh-tw"), ("gr", "el"), ("dk", "da"),
    ("se", "sv"), ("ua", "uk"), ("cz", "cs"), ("il", "he"), ("vn", "vi"), ("br", "pt"), ("ir", "fa"), ("kz", "kk"), ("gb", "en"), ("us", "en"),
    ("rs", "sr"), ("pk", "ur"), ("by", "be"), ("al", "sq"), ("ge", "ka"), ("ee", "et"), ("ch", "de"), ("at", "de"),
];

fn lookup<V: Copy>(table: &'static [(&'static str, V)], key: &str) -> Option<V> {
    table.iter().find(|(k, _)| *k == key).map(|(_, v)| *v)
}

/// The `LANGUAGES` entry of a code: `(code, name)` with static lifetimes.
pub fn language_entry(code: &str) -> Option<(&'static str, &'static str)> {
    LANGUAGES.iter().find(|(k, _)| *k == code).copied()
}

/// `Object.hasOwn(LANGUAGES, code)`.
pub fn is_language(code: &str) -> bool {
    language_entry(code).is_some()
}

/// `langFlag(code)`: the flag emoji of a canonical code, or a globe when unknown.
pub fn lang_flag(code: &str) -> String {
    let Some(country) = lookup(LANG_FLAGS, code).flatten() else { return "🌐".to_string() };
    country.to_uppercase().chars().filter_map(|c| char::from_u32(0x1F1E6 + (c as u32) - 65)).collect()
}

fn is_lower_alpha(s: &str, min: usize, max: usize) -> bool {
    (min..=max).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_lowercase())
}

/// `normalizeLangCode(code)`: the canonical language code for any code a provider or user may produce, or `None`.
/// Keeps Traditional Chinese (`zh-tw`) apart from Simplified (`zh`); the result may be a code that is not in
/// [`LANGUAGES`] (e.g. `ceb`).
pub fn normalize_lang_code(code: &str) -> Option<String> {
    let c = js::lower(js::trim(code)).replace('_', "-");
    // /^[a-z]{2,3}(?:-[a-z0-9]{2,8}){0,3}$/ (also rejects "auto")
    let parts: Vec<&str> = c.split('-').collect();
    let shape_ok = parts.len() <= 4
        && is_lower_alpha(parts[0], 2, 3)
        && parts[1..].iter().all(|p| (2..=8).contains(&p.len()) && p.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()));
    if !shape_ok || c == "und" {
        return None;
    }
    if let Some(a) = lookup(CODE_ALIASES, &c) {
        return Some(a.to_string());
    }
    if is_language(&c) {
        return Some(c);
    }
    let base = parts[0];
    let subs = &parts[1..];
    if base == "zh" {
        return Some(if subs.iter().any(|s| matches!(*s, "hant" | "tw" | "hk" | "mo")) { "zh-tw" } else { "zh" }.to_string());
    }
    if let Some(a) = lookup(CODE_ALIASES, base) {
        return Some(a.to_string());
    }
    Some(base.to_string())
}

static INTL_NAMES: LazyLock<HashMap<&'static str, &'static str>> = LazyLock::new(|| crate::intl_data::LANGUAGE_NAMES.iter().copied().collect());
static LATIN_CODES: LazyLock<HashSet<&'static str>> = LazyLock::new(|| crate::intl_data::LATIN_SCRIPT_CODES.iter().copied().collect());

/// `languageName(code)`: the English name; the raw code when unknown, `Unknown` for none. Codes outside
/// [`LANGUAGES`] use ICU's English display names (generated table).
pub fn language_name(code: Option<&str>) -> String {
    let Some(code) = code.filter(|c| !c.is_empty()) else { return "Unknown".to_string() };
    if let Some((_, name)) = language_entry(code) {
        return name.to_string();
    }
    INTL_NAMES.get(code).map(|n| n.to_string()).unwrap_or_else(|| code.to_string())
}

/// `resolveLanguageInput(input)`: a [`LANGUAGES`] key for a code ("ZH-TW", "iw", "zh-Hant") or an English name
/// ("Japanese"), else `None`.
pub fn resolve_language_input(input: &str) -> Option<&'static str> {
    if let Some(n) = normalize_lang_code(input)
        && let Some((code, _)) = language_entry(&n)
    {
        return Some(code);
    }
    resolve_language_name(input)
}

/// The name half of `resolveLanguageInput`: `String(input).trim().toLowerCase()` against the English names and
/// [`NAME_ALIASES`]. A value that is not a string only gets this half (`normalizeLangCode` refuses non-strings).
fn resolve_language_name(input: &str) -> Option<&'static str> {
    let name = js::lower(js::trim(input));
    if name.is_empty() {
        return None;
    }
    for (code, label) in LANGUAGES {
        if js::lower(label) == name {
            return Some(code);
        }
    }
    lookup(NAME_ALIASES, &name)
}

/// `suggestLanguage(input)`: a likely intended code for a country code such as "jp".
pub fn suggest_language(input: &str) -> Option<&'static str> {
    lookup(DID_YOU_MEAN, &js::lower(js::trim(input)))
}

/// The result of [`normalize_target_list`].
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize)]
pub struct TargetList {
    pub valid: Vec<String>,
    /// The raw input of entries that are not a known language.
    pub invalid: Vec<String>,
}

/// `normalizeTargetList(inputs)`: canonicalises and de-duplicates a list of user inputs (a repeat is dropped
/// silently).
pub fn normalize_target_list<S: AsRef<str>>(inputs: &[S]) -> TargetList {
    let mut out = TargetList::default();
    for input in inputs {
        match resolve_language_input(input.as_ref()) {
            None => out.invalid.push(input.as_ref().to_string()),
            Some(code) => {
                if !out.valid.iter().any(|v| v == code) {
                    out.valid.push(code.to_string());
                }
            }
        }
    }
    out
}

/// [`normalize_target_list`] over stored JSON (`targetLangs` as the cache or a legacy row holds it): a value that
/// is not a string is never a language.
fn normalize_target_values(inputs: &[Value]) -> Vec<String> {
    let mut valid: Vec<String> = Vec::new();
    for input in inputs {
        let code = match input {
            Value::String(s) => resolve_language_input(s),
            // `normalizeLangCode` returns null for a non-string, so only `String(input)` as a name can match.
            Value::Null => None,
            other => resolve_language_name(&js_to_string(other)),
        };
        if let Some(code) = code
            && !valid.iter().any(|v| v == code)
        {
            valid.push(code.to_string());
        }
    }
    valid
}

/// `toGoogleCode(code)`: the code Google expects.
pub fn to_google_code(code: &str) -> &str {
    match code {
        "zh" => "zh-CN",
        "zh-tw" => "zh-TW",
        other => other,
    }
}

// ── JS value helpers ────────────────────────────────────────────────────────────

/// `String(value)` as JS writes it (`null` -> "null", objects -> "[object Object]").
fn js_to_string(v: &Value) -> String {
    match v {
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => js::number_to_string(n.as_f64().unwrap_or(0.0)),
        Value::String(s) => s.clone(),
        Value::Array(items) => items.iter().map(|i| if i.is_null() { String::new() } else { js_to_string(i) }).collect::<Vec<_>>().join(","),
        Value::Object(_) => "[object Object]".into(),
    }
}

/// `value?.[0]` (an array element, the first UTF-16 unit of a string, the "0" key of an object).
fn js_index0(v: &Value) -> Option<Value> {
    match v {
        Value::Array(items) => items.first().cloned(),
        Value::String(s) => s.encode_utf16().next().map(|u| Value::String(String::from_utf16_lossy(&[u]))),
        Value::Object(o) => o.get("0").cloned(),
        _ => None,
    }
}

/// `Number(text)` for a header value: trimmed, empty is 0, decimal, exponent, `Infinity` and `0x`/`0o`/`0b`
/// integers; anything else is NaN.
pub fn js_number(text: Option<&str>) -> f64 {
    let Some(raw) = text else { return 0.0 };
    let s = js::trim(raw);
    if s.is_empty() {
        return 0.0;
    }
    match s {
        "Infinity" | "+Infinity" => return f64::INFINITY,
        "-Infinity" => return f64::NEG_INFINITY,
        _ => {}
    }
    for (prefix, radix) in [("0x", 16), ("0X", 16), ("0o", 8), ("0O", 8), ("0b", 2), ("0B", 2)] {
        if let Some(digits) = s.strip_prefix(prefix) {
            return if !digits.is_empty() && digits.chars().all(|c| c.is_digit(radix)) {
                digits.chars().fold(0.0, |acc, c| acc * f64::from(radix) + f64::from(c.to_digit(radix).unwrap_or(0)))
            } else {
                f64::NAN
            };
        }
    }
    let body = s.strip_prefix(['+', '-']).unwrap_or(s);
    let (mantissa, exp) = match body.find(['e', 'E']) {
        Some(i) => (&body[..i], Some(&body[i + 1..])),
        None => (body, None),
    };
    let mant_ok = {
        let mut parts = mantissa.splitn(2, '.');
        let int = parts.next().unwrap_or("");
        let frac = parts.next();
        int.bytes().all(|b| b.is_ascii_digit())
            && frac.is_none_or(|f| f.bytes().all(|b| b.is_ascii_digit()))
            && (!int.is_empty() || frac.is_some_and(|f| !f.is_empty()))
    };
    let exp_ok = exp.is_none_or(|e| {
        let d = e.strip_prefix(['+', '-']).unwrap_or(e);
        !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit())
    });
    if !mant_ok || !exp_ok {
        return f64::NAN;
    }
    s.parse::<f64>().unwrap_or(f64::NAN)
}

// ── Provider configuration ─────────────────────────────────────────────────────

/// The translation backend (`TRANSLATE_PROVIDER`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    Google,
    LibreTranslate,
}

impl Provider {
    /// `'google'` or `'libretranslate'` (used in cache keys).
    pub fn as_str(self) -> &'static str {
        match self {
            Provider::Google => "google",
            Provider::LibreTranslate => "libretranslate",
        }
    }

    fn pause_ms(self) -> f64 {
        match self {
            Provider::Google => PAUSE_MS_GOOGLE,
            Provider::LibreTranslate => PAUSE_MS_LIBRETRANSLATE,
        }
    }
}

/// The effective configuration (`config()`): the environment, then [`configure`] overrides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderConfig {
    pub provider: Provider,
    pub lt_url: String,
    pub lt_key: String,
    pub lt_fallback_url: String,
    pub result_cache: bool,
}

/// Overrides for [`configure`] (the JS `__testing.configure`): unset fields keep their value.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConfigOverride {
    pub provider: Option<Provider>,
    pub lt_url: Option<String>,
    pub lt_key: Option<String>,
    pub lt_fallback_url: Option<String>,
    pub result_cache: Option<bool>,
}

fn strip_trailing_slashes(s: &str) -> String {
    s.trim_end_matches('/').to_string()
}

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

fn env_config() -> ProviderConfig {
    let raw_env = std::env::var("TRANSLATE_PROVIDER").unwrap_or_default();
    let raw = js::trim(&raw_env);
    let mut provider = Provider::Google;
    if !raw.is_empty() && js::lower(raw) == "libretranslate" {
        provider = Provider::LibreTranslate;
    } else if !raw.is_empty() && js::lower(raw) != "google" {
        bot_util::log_warn!(format!("TRANSLATE_PROVIDER=\"{raw}\" is not supported; using google."));
    }
    ProviderConfig {
        provider,
        lt_url: strip_trailing_slashes(&env_nonempty("LIBRETRANSLATE_URL").unwrap_or_else(|| "http://localhost:5000".into())),
        lt_key: env_nonempty("LIBRETRANSLATE_API_KEY").unwrap_or_default(),
        lt_fallback_url: strip_trailing_slashes(&env_nonempty("LIBRETRANSLATE_FALLBACK_URL").unwrap_or_default()),
        result_cache: true,
    }
}

// ── HTTP transport ──────────────────────────────────────────────────────────────

/// One HTTP request (what `fetch(url, init)` was given).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpRequest {
    pub method: &'static str,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<String>,
    /// `AbortSignal.timeout(ms)`: the whole request, body included.
    pub timeout_ms: u64,
}

/// A response: status, headers and the body (an `Err` when reading it failed).
#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Result<Vec<u8>, String>,
}

impl HttpResponse {
    /// A response with a JSON body.
    pub fn json(status: u16, body: &Value) -> Self {
        Self {
            status,
            headers: vec![("content-type".into(), "application/json".into())],
            body: Ok(serde_json::to_vec(body).unwrap_or_default()),
        }
    }

    /// A response with a text body.
    pub fn text(status: u16, body: &str) -> Self {
        Self { status, headers: Vec::new(), body: Ok(body.as_bytes().to_vec()) }
    }

    /// Adds a header.
    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }

    /// `res.headers.get(name)` (case-insensitive).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    /// `res.ok`.
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// `await res.json()`.
    fn parse_json(&self) -> Result<Value, String> {
        let bytes = self.body.as_ref().map_err(Clone::clone)?;
        let text = String::from_utf8_lossy(bytes);
        let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
        serde_json::from_str(text).map_err(|e| e.to_string())
    }
}

/// Why a request got no response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportError {
    /// The timeout elapsed (`AbortError` / `TimeoutError`).
    Timeout,
    /// Any other failure, with its message.
    Network(String),
}

/// Sends HTTP requests (`fetch`). Production uses [`ReqwestTransport`]; tests install canned responses.
#[async_trait::async_trait]
pub trait HttpTransport: Send + Sync {
    async fn send(&self, req: HttpRequest) -> Result<HttpResponse, TransportError>;
}

/// The production transport.
pub struct ReqwestTransport {
    client: reqwest::Client,
}

impl Default for ReqwestTransport {
    fn default() -> Self {
        Self { client: reqwest::Client::builder().build().unwrap_or_default() }
    }
}

#[async_trait::async_trait]
impl HttpTransport for ReqwestTransport {
    async fn send(&self, req: HttpRequest) -> Result<HttpResponse, TransportError> {
        let method = reqwest::Method::from_bytes(req.method.as_bytes()).unwrap_or(reqwest::Method::GET);
        let mut builder = self.client.request(method, &req.url).timeout(std::time::Duration::from_millis(req.timeout_ms));
        for (k, v) in &req.headers {
            builder = builder.header(k.as_str(), v.as_str());
        }
        if let Some(body) = req.body {
            builder = builder.body(body);
        }
        let res = builder.send().await.map_err(|e| if e.is_timeout() { TransportError::Timeout } else { TransportError::Network(e.to_string()) })?;
        let status = res.status().as_u16();
        let headers = res.headers().iter().map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or_default().to_string())).collect();
        let body = res.bytes().await.map(|b| b.to_vec()).map_err(|e| e.to_string());
        Ok(HttpResponse { status, headers, body })
    }
}

// ── Errors ────────────────────────────────────────────────────────────────────

/// `TranslationError.code`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    Paused,
    RateLimited,
    UnsupportedLanguage,
    Empty,
    TooLong,
    Timeout,
    Network,
    HttpError,
    BadResponse,
}

impl ErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorCode::Paused => "PAUSED",
            ErrorCode::RateLimited => "RATE_LIMITED",
            ErrorCode::UnsupportedLanguage => "UNSUPPORTED_LANGUAGE",
            ErrorCode::Empty => "EMPTY",
            ErrorCode::TooLong => "TOO_LONG",
            ErrorCode::Timeout => "TIMEOUT",
            ErrorCode::Network => "NETWORK",
            ErrorCode::HttpError => "HTTP_ERROR",
            ErrorCode::BadResponse => "BAD_RESPONSE",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "PAUSED" => ErrorCode::Paused,
            "RATE_LIMITED" => ErrorCode::RateLimited,
            "UNSUPPORTED_LANGUAGE" => ErrorCode::UnsupportedLanguage,
            "EMPTY" => ErrorCode::Empty,
            "TOO_LONG" => ErrorCode::TooLong,
            "TIMEOUT" => ErrorCode::Timeout,
            "NETWORK" => ErrorCode::Network,
            "HTTP_ERROR" => ErrorCode::HttpError,
            "BAD_RESPONSE" => ErrorCode::BadResponse,
            _ => return None,
        })
    }
}

/// `TranslationError`.
#[derive(Debug, Clone, PartialEq)]
pub struct TranslationError {
    pub code: ErrorCode,
    pub message: String,
    pub status: Option<u16>,
    pub retry_after_ms: Option<f64>,
    pub lang: Option<String>,
}

impl TranslationError {
    /// `new TranslationError(code, message)`.
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self { code, message: message.into(), status: None, retry_after_ms: None, lang: None }
    }

    /// `new TranslationError(code)` (the message is the code).
    pub fn of(code: ErrorCode) -> Self {
        Self::new(code, code.as_str())
    }

    pub fn status(mut self, status: u16) -> Self {
        self.status = Some(status);
        self
    }

    pub fn retry_after(mut self, ms: f64) -> Self {
        self.retry_after_ms = Some(ms);
        self
    }

    pub fn lang(mut self, lang: impl Into<String>) -> Self {
        self.lang = Some(lang.into());
        self
    }
}

impl std::fmt::Display for TranslationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for TranslationError {}

// ── Mutable state (per app) ─────────────────────────────────────────────────────

/// The per-app translation channel list: channel id -> canonical targets, in the JS `Map` order.
pub type ChannelTargets = IndexMap<String, Vec<String>>;

type LoadFuture = Shared<BoxFuture<'static, Option<Vec<String>>>>;

#[derive(Default)]
struct LtSupport {
    codes: Vec<String>,
    raw_codes: HashSet<String>,
    loaded_at: i64,
    failed_at: i64,
    inflight: Option<LoadFuture>,
    known: bool,
    via_fallback: bool,
}

#[derive(Default)]
struct Inner {
    paused_until: f64,
    last_pause_sync: i64,
    lt: LtSupport,
    /// Bumped by [`reset`]: a load that began before it never writes (the JS `state === ltSupport`).
    lt_epoch: u64,
    lt_main_down_until: i64,
    lt_on_fallback: bool,
    runtime_unsupported: HashSet<String>,
    channel_memo: IndexMap<String, (i64, ChannelTargets)>,
    channel_gen: IndexMap<String, u64>,
    auto_inflight: usize,
    google_blocked_until: HashMap<String, i64>,
}

struct Throttles {
    ten_min: bot_util::async_util::Throttle,
    hour: bot_util::async_util::Throttle,
    minute: bot_util::async_util::Throttle,
}

impl Throttles {
    /// `makeThrottles()` on the app clock (`createThrottle`), so tests move them with the `ManualClock`.
    fn new(app: &App) -> Self {
        let c = app.clock();
        Self {
            ten_min: bot_core::clock::clock_throttle(c, 10 * 60_000),
            hour: bot_core::clock::clock_throttle(c, 60 * 60_000),
            minute: bot_core::clock::clock_throttle(c, 60_000),
        }
    }
}

/// The JS module state of translationService, one per app (`app.ext::<TranslationState>()`).
#[derive(Default)]
pub struct TranslationState {
    env: Mutex<Option<ProviderConfig>>,
    over: Mutex<ConfigOverride>,
    transport: Mutex<Option<Arc<dyn HttpTransport>>>,
    inner: Mutex<Inner>,
    /// Built on first use (they need the app clock); `reset` drops them.
    throttles: Mutex<Option<Arc<Throttles>>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn state(app: &App) -> Arc<TranslationState> {
    app.ext::<TranslationState>()
}

fn inner(app: &App) -> Arc<TranslationState> {
    state(app)
}

fn throttles(app: &App) -> Arc<Throttles> {
    let st = state(app);
    let mut g = lock(&st.throttles);
    g.get_or_insert_with(|| Arc::new(Throttles::new(app))).clone()
}

fn now(app: &App) -> i64 {
    app.clock().now_ms()
}

/// `__testing.configure(opts)`: overrides the lazy env config (unset fields are kept).
pub fn configure(app: &App, opts: ConfigOverride) {
    let st = state(app);
    let mut o = lock(&st.over);
    if opts.provider.is_some() {
        o.provider = opts.provider;
    }
    if opts.lt_url.is_some() {
        o.lt_url = opts.lt_url;
    }
    if opts.lt_key.is_some() {
        o.lt_key = opts.lt_key;
    }
    if opts.lt_fallback_url.is_some() {
        o.lt_fallback_url = opts.lt_fallback_url;
    }
    if opts.result_cache.is_some() {
        o.result_cache = opts.result_cache;
    }
}

/// `__testing.reset()`: forgets every piece of state (pause, LibreTranslate support, failover, memo, throttles,
/// overrides and the env config). The transport is kept.
pub fn reset(app: &App) {
    let st = state(app);
    {
        let mut g = lock(&st.inner);
        let epoch = g.lt_epoch + 1;
        *g = Inner::default();
        g.lt_epoch = epoch;
    }
    *lock(&st.throttles) = None;
    *lock(&st.over) = ConfigOverride::default();
    *lock(&st.env) = None;
}

/// Replaces the HTTP transport (tests serve canned responses through it).
pub fn set_transport(app: &App, transport: Arc<dyn HttpTransport>) {
    *lock(&state(app).transport) = Some(transport);
}

fn transport(app: &App) -> Arc<dyn HttpTransport> {
    let st = state(app);
    let mut t = lock(&st.transport);
    t.get_or_insert_with(|| Arc::new(ReqwestTransport::default())).clone()
}

/// `config()`: the env config (read once), then the overrides.
pub fn config(app: &App) -> ProviderConfig {
    let st = state(app);
    let base = lock(&st.env).get_or_insert_with(env_config).clone();
    let o = lock(&st.over).clone();
    ProviderConfig {
        provider: o.provider.unwrap_or(base.provider),
        lt_url: o.lt_url.unwrap_or(base.lt_url),
        lt_key: o.lt_key.unwrap_or(base.lt_key),
        lt_fallback_url: o.lt_fallback_url.unwrap_or(base.lt_fallback_url),
        result_cache: o.result_cache.unwrap_or(base.result_cache),
    }
}

/// `providerName()`.
pub fn provider_name(app: &App) -> Provider {
    config(app).provider
}

/// `providerLabel()`.
pub fn provider_label(app: &App) -> &'static str {
    if provider_name(app) == Provider::LibreTranslate { "LibreTranslate" } else { "Google Translate (keyless)" }
}

async fn do_fetch(app: &App, req: HttpRequest) -> Result<HttpResponse, TranslationError> {
    match transport(app).send(req).await {
        Ok(res) => Ok(res),
        Err(TransportError::Timeout) => Err(TranslationError::new(ErrorCode::Timeout, "The request timed out")),
        Err(TransportError::Network(msg)) => Err(TranslationError::new(ErrorCode::Network, msg)),
    }
}

// ── Circuit breaker ───────────────────────────────────────────────────────────

/// `translationPaused()`: synchronous; used by the hook filter and by the commands before they defer.
pub fn translation_paused(app: &App) -> bool {
    (now(app) as f64) < lock(&inner(app).inner).paused_until
}

/// `pauseRemainingMs()`.
pub fn pause_remaining_ms(app: &App) -> f64 {
    (lock(&inner(app).inner).paused_until - now(app) as f64).max(0.0)
}

/// The cache key of the shared pause (`translate:{provider}:pausedUntil`).
pub fn paused_key(provider: Provider) -> String {
    format!("translate:{}:pausedUntil", provider.as_str())
}

async fn trip_breaker(app: &App, res: &HttpResponse) -> TranslationError {
    let provider = provider_name(app);
    let mut pause_ms = provider.pause_ms();
    let retry_after = js_number(res.header("retry-after"));
    if retry_after.is_finite() && retry_after * 1000.0 > pause_ms {
        pause_ms = (retry_after * 1000.0).min(PAUSE_MAX_MS);
    }
    let was_paused = translation_paused(app);
    let paused_until = {
        let st = inner(app);
        let mut g = lock(&st.inner);
        let until = now(app) as f64 + pause_ms;
        if until > g.paused_until {
            g.paused_until = until;
        }
        g.paused_until
    };
    if !was_paused {
        bot_util::log_warn!(format!(
            "translationService: {} returned 429; pausing translation for {} min",
            provider.as_str(),
            (pause_ms / 60_000.0).ceil()
        ));
    }
    // Fail-open: when Redis is down the in-process pause still holds; the key only shares it across shards.
    app.cache().set(&paused_key(provider), &bot_core::command::js_number(paused_until), (pause_ms / 1000.0).ceil()).await;
    TranslationError::new(ErrorCode::RateLimited, "Rate limited").status(429).retry_after(pause_ms)
}

async fn assert_not_paused(app: &App) -> Result<(), TranslationError> {
    let t = now(app);
    let sync = {
        let st = inner(app);
        let mut g = lock(&st.inner);
        if t - g.last_pause_sync >= 60_000 {
            g.last_pause_sync = t;
            true
        } else {
            false
        }
    };
    if sync {
        let shared = app.cache().get(&paused_key(provider_name(app))).await;
        if let Some(n) = shared.as_ref().and_then(Value::as_f64).filter(|n| n.is_finite()) {
            let st = inner(app);
            let mut g = lock(&st.inner);
            if n > g.paused_until {
                g.paused_until = n;
            }
        }
    }
    if translation_paused(app) {
        return Err(TranslationError::new(ErrorCode::Paused, "Translation is paused after a rate limit").retry_after(pause_remaining_ms(app)));
    }
    Ok(())
}

// ── Providers ─────────────────────────────────────────────────────────────────

/// A translation and the language the provider detected.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Translated {
    pub text: String,
    pub detected: Option<String>,
}

/// One translation through the keyless Google endpoint. GET while `q` is at most MAX_Q_ENCODED_BYTES once encoded;
/// one POST (form body) above that.
async fn google_translate(app: &App, text: &str, target: &str, source: &str) -> Result<Translated, TranslationError> {
    let q = js::encode_uri_component(text);
    if q.len() > MAX_POST_Q_BYTES {
        return Err(TranslationError::new(ErrorCode::TooLong, "Text is too long for one request"));
    }
    let post = q.len() > MAX_Q_ENCODED_BYTES;
    let sl = js::encode_uri_component(if source == "auto" { "auto" } else { to_google_code(source) });
    let tl = js::encode_uri_component(to_google_code(target));

    let t = now(app);
    let order: Vec<&str> = {
        let st = inner(app);
        let g = lock(&st.inner);
        let open: Vec<&str> = GOOGLE_CLIENTS.iter().copied().filter(|c| g.google_blocked_until.get(*c).copied().unwrap_or(0) <= t).collect();
        if open.is_empty() { GOOGLE_CLIENTS.to_vec() } else { open }
    };
    let mut limited: Option<HttpResponse> = None;
    let mut rejected: Option<u16> = None;
    for (i, client) in order.iter().enumerate() {
        let base = format!("{GOOGLE_ENDPOINT}?client={client}&sl={sl}&tl={tl}&dt=t");
        let req = if post {
            HttpRequest {
                method: "POST",
                url: base,
                headers: vec![
                    ("User-Agent".into(), "Mozilla/5.0".into()),
                    ("Content-Type".into(), "application/x-www-form-urlencoded;charset=UTF-8".into()),
                ],
                body: Some(format!("q={q}")),
                timeout_ms: GOOGLE_TIMEOUT_MS,
            }
        } else {
            HttpRequest {
                method: "GET",
                url: format!("{base}&q={q}"),
                headers: vec![("User-Agent".into(), "Mozilla/5.0".into())],
                body: None,
                timeout_ms: GOOGLE_TIMEOUT_MS,
            }
        };
        let res = do_fetch(app, req).await?;
        if res.status == 429 {
            let blocked = now(app) + GOOGLE_CLIENT_BLOCK_MS;
            lock(&inner(app).inner).google_blocked_until.insert(client.to_string(), blocked);
            if i < order.len() - 1 && throttles(app).ten_min.allow(&format!("google429:{client}")) {
                bot_util::log_warn!(format!("translationService: google client \"{client}\" returned 429; trying \"{}\"", order[i + 1]));
            }
            limited = Some(res);
            continue;
        }
        if !res.ok() {
            if post && (400..500).contains(&res.status) && i < order.len() - 1 {
                rejected = Some(res.status);
                continue;
            }
            return Err(TranslationError::new(ErrorCode::HttpError, format!("Google returned {}", res.status)).status(res.status));
        }
        let data = res.parse_json().map_err(|_| TranslationError::new(ErrorCode::BadResponse, "Google returned invalid JSON"))?;
        let Some(segments) = data.as_array().and_then(|a| a.first()).and_then(Value::as_array) else {
            return Err(TranslationError::new(ErrorCode::BadResponse, "Unexpected Google response"));
        };
        lock(&inner(app).inner).google_blocked_until.remove(*client);
        let text: String = segments
            .iter()
            .map(|seg| match js_index0(seg) {
                None | Some(Value::Null) => String::new(),
                Some(v) => js_to_string(&v),
            })
            .collect();
        let detected = data.as_array().and_then(|a| a.get(2)).and_then(Value::as_str).and_then(normalize_lang_code);
        return Ok(Translated { text, detected });
    }
    if let Some(res) = limited {
        return Err(trip_breaker(app, &res).await);
    }
    Err(TranslationError::new(ErrorCode::HttpError, format!("Google returned {}", rejected.map(|s| s.to_string()).unwrap_or_else(|| "an error".into())))
        .with_status_opt(rejected))
}

impl TranslationError {
    fn with_status_opt(mut self, status: Option<u16>) -> Self {
        self.status = status;
        self
    }
}

/// `toLtCode(code, rawCodes)`: the LibreTranslate server code for a canonical code (no -> nb, zh -> zh-Hans,
/// zh-tw -> zh-Hant ...), chosen from the server's own list (lower-case codes) when it is known.
pub fn to_lt_code(code: &str, raw_codes: &HashSet<String>) -> String {
    let candidates: Vec<&str> = match code {
        "zh" => vec!["zh-Hans", "zh"],
        "zh-tw" => vec!["zh-Hant", "zt"],
        "no" => vec!["nb", "no"],
        "he" => vec!["he", "iw"],
        "tl" => vec!["tl", "fil"],
        "pt" => vec!["pt", "pt-BR"],
        other => vec![other],
    };
    if !raw_codes.is_empty()
        && let Some(hit) = candidates.iter().find(|c| raw_codes.contains(&c.to_lowercase()))
    {
        return hit.to_string();
    }
    candidates[0].to_string()
}

/// [`to_lt_code`] with the loaded server list.
pub fn to_lt_code_loaded(app: &App, code: &str) -> String {
    let raw = lock(&inner(app).inner).lt.raw_codes.clone();
    to_lt_code(code, &raw)
}

/// A LibreTranslate request to LIBRETRANSLATE_URL, or to LIBRETRANSLATE_FALLBACK_URL while that one is down (network
/// error, timeout or 502/503/504 from the main server). Returns the response and whether the fallback answered.
async fn lt_fetch(app: &App, path: &str, make: impl Fn(String) -> HttpRequest) -> Result<(HttpResponse, bool), TranslationError> {
    let cfg = config(app);
    let (lt_url, fallback) = (cfg.lt_url, cfg.lt_fallback_url);
    if fallback.is_empty() || fallback == lt_url {
        return Ok((do_fetch(app, make(format!("{lt_url}{path}"))).await?, false));
    }
    let via_backup = || async { Ok((do_fetch(app, make(format!("{fallback}{path}"))).await?, true)) };
    if now(app) < lock(&inner(app).inner).lt_main_down_until {
        return via_backup().await;
    }
    let reason = match do_fetch(app, make(format!("{lt_url}{path}"))).await {
        Ok(res) => {
            if !LT_FAILOVER_STATUS.contains(&res.status) {
                let was = std::mem::replace(&mut lock(&inner(app).inner).lt_on_fallback, false);
                if was {
                    bot_util::log_info!("translationService: the main LibreTranslate server answers again; the fallback is no longer used");
                }
                return Ok((res, false));
            }
            format!("HTTP {}", res.status)
        }
        Err(e) if matches!(e.code, ErrorCode::Network | ErrorCode::Timeout) => e.message,
        Err(e) => return Err(e),
    };
    let newly = {
        let st = inner(app);
        let mut g = lock(&st.inner);
        g.lt_main_down_until = now(app) + LT_FAILOVER_MS;
        !std::mem::replace(&mut g.lt_on_fallback, true)
    };
    if newly && throttles(app).ten_min.allow("lt-failover") {
        bot_util::log_warn!(format!("translationService: the main LibreTranslate server failed ({reason}); using the fallback"));
    }
    via_backup().await
}

async fn lt_post(app: &App, path: &str, body: Value) -> Result<(HttpResponse, bool), TranslationError> {
    let key = config(app).lt_key;
    let mut body = body;
    if !key.is_empty()
        && let Some(o) = body.as_object_mut()
    {
        o.insert("api_key".into(), Value::String(key));
    }
    let payload = serde_json::to_string(&body).unwrap_or_default();
    let out = lt_fetch(app, path, |url| HttpRequest {
        method: "POST",
        url,
        headers: vec![("Content-Type".into(), "application/json".into())],
        body: Some(payload.clone()),
        timeout_ms: LT_TIMEOUT_MS,
    })
    .await?;
    if out.0.status == 429 {
        return Err(trip_breaker(app, &out.0).await);
    }
    Ok(out)
}

static LT_UNSUPPORTED_RE: LazyRe = lazy_re!(r"not supported|unsupported|invalid (source|target)? ?language", "i");
static SOURCE_RE: LazyRe = lazy_re!("source", "i");
static TARGET_RE: LazyRe = lazy_re!("target", "i");

/// LibreTranslate answers `en` with confidence 0 when none of its loaded languages matches: that is "unknown".
fn lt_language(found: Option<&Value>) -> Option<String> {
    let found = found?;
    if let Some(c) = found.get("confidence").and_then(Value::as_f64)
        && c <= 0.0
    {
        return None;
    }
    found.get("language").and_then(Value::as_str).and_then(normalize_lang_code)
}

async fn lt_translate(app: &App, text: &str, target: &str, source: &str) -> Result<(Translated, bool), TranslationError> {
    let body = json!({
        "q": text,
        "source": if source == "auto" { "auto".to_string() } else { to_lt_code_loaded(app, source) },
        "target": to_lt_code_loaded(app, target),
        "format": "text",
    });
    let (res, via_fallback) = lt_post(app, "/translate", body).await?;
    if !res.ok() {
        if res.status == 400 || res.status == 403 {
            let body = res.parse_json().ok();
            let message = match body.as_ref().and_then(|b| b.get("error")) {
                None | Some(Value::Null) => String::new(),
                Some(v) => js_to_string(v),
            };
            if LT_UNSUPPORTED_RE.test(&message) {
                let names_source = source != "auto" && SOURCE_RE.test(&message) && !TARGET_RE.test(&message);
                let lang = if names_source { source } else { target };
                // The fallback may lack a language the main server has: fail this request only, remember nothing.
                if via_fallback {
                    return Err(TranslationError::new(ErrorCode::HttpError, format!("The fallback LibreTranslate server lacks {lang}: {message}"))
                        .status(res.status)
                        .lang(lang));
                }
                lock(&inner(app).inner).runtime_unsupported.insert(lang.to_string());
                return Err(TranslationError::new(ErrorCode::UnsupportedLanguage, message).status(res.status).lang(lang));
            }
        }
        return Err(TranslationError::new(ErrorCode::HttpError, format!("LibreTranslate returned {}", res.status)).status(res.status));
    }
    let data = res.parse_json().map_err(|_| TranslationError::new(ErrorCode::BadResponse, "LibreTranslate returned invalid JSON"))?;
    if !(data.is_object() || data.is_array()) {
        return Err(TranslationError::new(ErrorCode::BadResponse, "Unexpected LibreTranslate response"));
    }
    let text = data.get("translatedText").and_then(Value::as_str).unwrap_or_default().to_string();
    let detected = if source == "auto" { lt_language(data.get("detectedLanguage")) } else { Some(source.to_string()) };
    Ok((Translated { text, detected }, via_fallback))
}

async fn lt_detect(app: &App, text: &str) -> Result<Option<String>, TranslationError> {
    let (res, _) = lt_post(app, "/detect", json!({ "q": text })).await?;
    if !res.ok() {
        return Err(TranslationError::new(ErrorCode::HttpError, format!("LibreTranslate returned {}", res.status)).status(res.status));
    }
    let data = res.parse_json().map_err(|_| TranslationError::new(ErrorCode::BadResponse, "LibreTranslate returned invalid JSON"))?;
    Ok(lt_language(data.as_array().and_then(|a| a.first())))
}

/// `loadLanguageSupport({ force })`: the language codes a LibreTranslate server supports (canonical, in the
/// server's order). Google: `None` at once (everything in LANGUAGES works). Never fails; `None` also means "could
/// not load" (retried after LT_LANGUAGES_RETRY_MS). Concurrent calls share one request.
pub async fn load_language_support(app: &App, force: bool) -> Option<Vec<String>> {
    if provider_name(app) != Provider::LibreTranslate {
        return None;
    }
    let t = now(app);
    let fut = {
        let st = inner(app);
        let mut g = lock(&st.inner);
        if !force {
            // A list only the fallback gave (cold start while the main server is down) is re-fetched sooner.
            let ttl = if g.lt.via_fallback { LT_LANGUAGES_RETRY_MS } else { LT_LANGUAGES_TTL_MS };
            if g.lt.known && t - g.lt.loaded_at < ttl {
                return Some(g.lt.codes.clone());
            }
            // A recent failure is not retried at once; a stale but known list keeps serving meanwhile.
            if g.lt.failed_at != 0 && t - g.lt.failed_at < LT_LANGUAGES_RETRY_MS {
                return if g.lt.known { Some(g.lt.codes.clone()) } else { None };
            }
        }
        match &g.lt.inflight {
            Some(f) => f.clone(),
            None => {
                let epoch = g.lt_epoch;
                let a = app.clone();
                let f: LoadFuture = async move { load_once(&a, epoch).await }.boxed().shared();
                g.lt.inflight = Some(f.clone());
                f
            }
        }
    };
    fut.await
}

async fn load_once(app: &App, epoch: u64) -> Option<Vec<String>> {
    let current = |g: &Inner| g.lt_epoch == epoch;
    let result: Result<Vec<String>, String> = async {
        let (res, via_fallback) = lt_fetch(app, "/languages", |url| HttpRequest {
            method: "GET",
            url,
            headers: Vec::new(),
            body: None,
            timeout_ms: LT_LANGUAGES_TIMEOUT_MS,
        })
        .await
        .map_err(|e| e.message)?;
        if !res.ok() {
            return Err(format!("HTTP {}", res.status));
        }
        let entries = res.parse_json()?;
        let Some(entries) = entries.as_array() else { return Err("unexpected response".into()) };
        let st = inner(app);
        let mut g = lock(&st.inner);
        // The fallback may load fewer languages: never let its list replace one the main server gave.
        if via_fallback && g.lt.known && !g.lt.via_fallback {
            if current(&g) {
                g.lt.failed_at = now(app);
            }
            return Ok(g.lt.codes.clone());
        }
        let code_of = |e: &Value| match e.get("code") {
            None | Some(Value::Null) => String::new(),
            Some(v) => js_to_string(v),
        };
        let raw_codes: HashSet<String> = entries.iter().map(|e| js::lower(&code_of(e))).filter(|c| !c.is_empty()).collect();
        let mut codes: Vec<String> = Vec::new();
        for e in entries {
            if let Some(c) = e.get("code").and_then(Value::as_str).and_then(normalize_lang_code)
                && is_language(&c)
                && !codes.contains(&c)
            {
                codes.push(c);
            }
        }
        if current(&g) {
            let inflight = g.lt.inflight.take();
            g.lt = LtSupport { codes: codes.clone(), raw_codes, loaded_at: now(app), failed_at: 0, inflight, known: true, via_fallback };
        }
        Ok(codes)
    }
    .await;
    let out = match result {
        Ok(codes) => Some(codes),
        Err(msg) => {
            {
                let st = inner(app);
                let mut g = lock(&st.inner);
                if current(&g) {
                    g.lt.failed_at = now(app);
                }
            }
            if throttles(app).ten_min.allow("lt-languages") {
                bot_util::log_warn!(format!("translationService: could not load the LibreTranslate language list: {msg}"));
            }
            None
        }
    };
    let st = inner(app);
    let mut g = lock(&st.inner);
    if current(&g) {
        g.lt.inflight = None;
    }
    out
}

/// `warmLanguageSupport()`: fire-and-forget [`load_language_support`] (autocomplete).
pub fn warm_language_support(app: &App) {
    let a = app.clone();
    tokio::spawn(async move {
        load_language_support(&a, false).await;
    });
}

/// `isSupportedTarget(code)`: `Some(true|false)`, or `None` when unknown.
pub fn is_supported_target(app: &App, code: &str) -> Option<bool> {
    if provider_name(app) != Provider::LibreTranslate {
        return Some(true);
    }
    let st = inner(app);
    let g = lock(&st.inner);
    if g.runtime_unsupported.contains(code) {
        return Some(false);
    }
    if g.lt.known { Some(g.lt.codes.iter().any(|c| c == code)) } else { None }
}

/// `getLanguages()`: the languages the active backend offers, in the LANGUAGES order (sync).
pub fn get_languages(app: &App) -> Vec<(&'static str, &'static str)> {
    let known = lock(&inner(app).inner).lt.known;
    if provider_name(app) != Provider::LibreTranslate || !known {
        return LANGUAGES.to_vec();
    }
    LANGUAGES.iter().copied().filter(|(code, _)| is_supported_target(app, code) != Some(false)).collect()
}

/// One autocomplete choice.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LanguageChoice {
    pub name: String,
    pub value: String,
}

/// `languageChoices(query)`: autocomplete choices for a language option: exact code, code prefix, name prefix,
/// name substring (at most 25).
pub fn language_choices(app: &App, query: &str) -> Vec<LanguageChoice> {
    let q = js::lower(js::trim(query));
    let mut scored: Vec<(u8, &str, &str)> = Vec::new();
    for (code, name) in get_languages(app) {
        let lower = js::lower(name);
        let score = if q.is_empty() {
            Some(2)
        } else if code == q || lookup(CODE_ALIASES, &q) == Some(code) {
            Some(0)
        } else if code.starts_with(&q) {
            Some(1)
        } else if lower.starts_with(&q) {
            Some(2)
        } else if lower.contains(&q) {
            Some(3)
        } else {
            None
        };
        if let Some(s) = score {
            scored.push((s, name, code));
        }
    }
    scored.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| bot_util::locale_compare(a.1, b.1)));
    scored.into_iter().take(25).map(|(_, name, code)| LanguageChoice { name: format!("{name} ({code})"), value: code.to_string() }).collect()
}

// ── Text preparation ────────────────────────────────────────────────────────────

static LINK_TOKEN: LazyRe = lazy_re!(r"^<?https?:\/\/[^\s<>]+>?$", "i");

/// `isLinkOnly(content)`: true when the message is nothing but links (linear: splits on whitespace and tests each
/// word).
pub fn is_link_only(content: &str) -> bool {
    let t = js::trim(content);
    if t.is_empty() {
        return false;
    }
    t.split(js::is_js_space).filter(|w| !w.is_empty()).all(|w| LINK_TOKEN.test(w))
}

static RE_CUSTOM_EMOJI: LazyRe = lazy_re!(r"<a?:\w{1,32}:\d{17,20}>", "");
static RE_MENTION: LazyRe = lazy_re!(r"<(?:@[!&]?|#)\d{17,20}>", "");
static RE_SLASH: LazyRe = lazy_re!(r"<\/[^:>\n]{1,96}:\d{17,20}>", "");
static RE_STAMP: LazyRe = lazy_re!(r"<t:-?\d{1,13}(?::[tTdDfFR])?>", "");
static RE_EVERYONE: LazyRe = lazy_re!(r"@(?:everyone|here)\b", "");
static RE_LINK: LazyRe = lazy_re!(r"<?https?:\/\/[^\s<>]+>?", "i");
static EMOJI_RESIDUE: LazyRe =
    lazy_re!(r"[\p{Extended_Pictographic}\p{Emoji_Modifier}\p{Regional_Indicator}‍︎️⃣\u{E0020}-\u{E007F}]", "u");
static RE_HSPACE: LazyRe = lazy_re!(r"[^\S\n]+", "");
static RE_NL_SPACE: LazyRe = lazy_re!(r" ?\n ?", "");
static RE_NL3: LazyRe = lazy_re!(r"\n{3,}", "");

/// `cleanForTranslation(content)`: removes everything not worth translating: links, mentions, timestamps, custom
/// emoji and emoji pieces.
pub fn clean_for_translation(content: &str) -> String {
    let mut s = RE_CUSTOM_EMOJI.replace_all(content, " ");
    s = RE_MENTION.replace_all(&s, " ");
    s = RE_SLASH.replace_all(&s, " ");
    s = RE_STAMP.replace_all(&s, " ");
    s = RE_EVERYONE.replace_all(&s, " ");
    s = RE_LINK.replace_all(&s, " ");
    s = EMOJI_RESIDUE.replace_all(&s, " ");
    s = RE_HSPACE.replace_all(&s, " ");
    s = RE_NL_SPACE.replace_all(&s, "\n");
    s = RE_NL3.replace_all(&s, "\n\n");
    js::trim(&s).to_string()
}

static LETTER: LazyRe = lazy_re!(r"\p{L}", "u");
static CJK_CHAR: LazyRe = lazy_re!(r"[\p{Script=Han}\p{Script=Hiragana}\p{Script=Katakana}\p{Script=Hangul}]", "u");

/// `hasTranslatableText(text)`: at least two letters, or any CJK character (single-character words exist).
pub fn has_translatable_text(text: &str) -> bool {
    let units = bot_util::jsre::u16s(text);
    if LETTER.find_all(&units).len() >= 2 {
        return true;
    }
    CJK_CHAR.test_u16(&units)
}

const CJK_END: [char; 3] = ['。', '！', '？'];
const LATIN_END: [char; 5] = ['.', '!', '?', '؟', '।'];

fn is_space_at(cps: &[char], i: i64) -> bool {
    i >= 0 && cps.get(i as usize).is_some_and(|c| js::is_js_space(*c))
}

fn char_at(cps: &[char], i: i64) -> Option<char> {
    if i < 0 { None } else { cps.get(i as usize).copied() }
}

#[derive(Debug, Clone, Copy)]
struct Cut {
    end: usize,
    next: usize,
    sep: &'static str,
}

/// A cut point in a code-point array, searched backwards from `limit` down to `min_idx`: a newline, then a CJK
/// sentence end, then a sentence end followed by whitespace, then any whitespace.
fn find_cut(cps: &[char], start: usize, limit: usize, min_idx: usize) -> Option<Cut> {
    let lo = min_idx.max(start + 1) as i64;
    let limit = limit as i64;
    let range = || (lo..=limit).rev();
    for b in range() {
        if char_at(cps, b) == Some('\n') {
            return Some(Cut { end: b as usize, next: b as usize + 1, sep: "\n" });
        }
    }
    for b in range() {
        if char_at(cps, b - 1).is_some_and(|c| CJK_END.contains(&c)) {
            return Some(Cut { end: b as usize, next: b as usize, sep: "" });
        }
    }
    for b in range() {
        if char_at(cps, b - 1).is_some_and(|c| LATIN_END.contains(&c)) && is_space_at(cps, b) {
            return Some(Cut { end: b as usize, next: b as usize + 1, sep: " " });
        }
    }
    for b in range() {
        if is_space_at(cps, b) {
            return Some(Cut { end: b as usize, next: b as usize + 1, sep: " " });
        }
    }
    None
}

fn enc_len(c: char) -> usize {
    let mut buf = [0u8; 4];
    js::encoded_len(c.encode_utf8(&mut buf))
}

/// The result of [`clamp_for_request`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Clamped {
    pub text: String,
    pub truncated: bool,
}

/// `clampForRequest(text, { maxCodePoints, maxEncodedBytes })`: cuts text to the request limits on a sentence or
/// word boundary. `None` bytes is `Infinity`.
pub fn clamp_for_request(text: &str, max_code_points: usize, max_encoded_bytes: Option<usize>) -> Clamped {
    let cps: Vec<char> = text.chars().collect();
    let mut bytes = 0usize;
    let mut n = 0usize;
    while n < cps.len() && n < max_code_points {
        let b = if max_encoded_bytes.is_some() { enc_len(cps[n]) } else { 0 };
        if max_encoded_bytes.is_some_and(|max| bytes + b > max) {
            break;
        }
        bytes += b;
        n += 1;
    }
    if n >= cps.len() {
        return Clamped { text: text.to_string(), truncated: false };
    }
    let cut = find_cut(&cps, 0, n, (n as f64 * 0.7).floor() as usize);
    let end = cut.map(|c| c.end).unwrap_or(n);
    let s: String = cps[..end].iter().collect();
    Clamped { text: js::trim_end(&s).to_string(), truncated: true }
}

/// One piece of [`split_for_requests`]: its text and what the consumed boundary stood for.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Piece {
    pub text: String,
    pub sep: String,
}

/// `splitForRequests(text, maxEncodedBytes)`: pieces whose URL-encoded size is at most `max_encoded_bytes`, on
/// sentence or word boundaries.
pub fn split_for_requests(text: &str, max_encoded_bytes: usize) -> Vec<Piece> {
    let cps: Vec<char> = text.chars().collect();
    let enc: Vec<usize> = cps.iter().map(|c| enc_len(*c)).collect();
    let mut pieces = Vec::new();
    let mut start = 0usize;
    while start < cps.len() {
        let mut bytes = 0usize;
        let mut end = start;
        while end < cps.len() && bytes + enc[end] <= max_encoded_bytes {
            bytes += enc[end];
            end += 1;
        }
        if end >= cps.len() {
            pieces.push(Piece { text: cps[start..].iter().collect(), sep: String::new() });
            break;
        }
        if end == start {
            end = start + 1; // a single code point above the limit: take it alone, never loop
        }
        let cut = find_cut(&cps, start, end, start + ((end - start) as f64 * 0.5).floor() as usize);
        let stop = cut.map(|c| c.end).unwrap_or(end);
        pieces.push(Piece { text: cps[start..stop].iter().collect(), sep: cut.map(|c| c.sep).unwrap_or("").to_string() });
        start = cut.map(|c| c.next).unwrap_or(end);
    }
    if pieces.is_empty() {
        pieces.push(Piece { text: String::new(), sep: String::new() });
    }
    pieces
}

/// `splitForEmbeds(text, max, maxParts)`: at most `max_parts` embed descriptions of at most `max` UTF-16 units; when
/// text is left over, the last part ends with an ellipsis.
pub fn split_for_embeds(text: &str, max: usize, max_parts: usize) -> Vec<String> {
    let cps: Vec<char> = text.chars().collect();
    let mut parts: Vec<String> = Vec::new();
    let mut start = 0usize;
    while start < cps.len() && parts.len() < max_parts {
        let mut len = 0usize;
        let mut end = start;
        while end < cps.len() && len + cps[end].len_utf16() <= max {
            len += cps[end].len_utf16();
            end += 1;
        }
        if end >= cps.len() {
            parts.push(cps[start..].iter().collect());
            break;
        }
        if parts.len() == max_parts - 1 {
            let rest: String = cps[start..].iter().collect();
            parts.push(bot_util::text::clamp_text_f64(&rest, max as f64 - 1.0, bot_util::text::ELLIPSIS));
            break;
        }
        if end == start {
            end = start + 1;
        }
        let cut = find_cut(&cps, start, end, start + ((end - start) as f64 * 0.5).floor() as usize);
        let stop = cut.map(|c| c.end).unwrap_or(end);
        parts.push(cps[start..stop].iter().collect());
        start = cut.map(|c| c.next).unwrap_or(end);
    }
    parts.iter().map(|p| js::trim(p).to_string()).filter(|p| !p.is_empty()).collect()
}

/// What auto-translate sends for a message.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct PreparedInput {
    pub text: String,
    pub truncated: bool,
    #[serde(rename = "hadSpoiler")]
    pub had_spoiler: bool,
}

/// `prepareAutoInput(content)` with the provider's request limit: cleaned, capped, with the spoiler flag. `None`:
/// nothing to translate.
pub fn prepare_auto_input_for(provider: Provider, content: &str) -> Option<PreparedInput> {
    let had_spoiler = content.matches("||").count() >= 2;
    let cleaned = clean_for_translation(&content.replace("||", " "));
    if !has_translatable_text(&cleaned) {
        return None;
    }
    let c = clamp_for_request(&cleaned, AUTO_INPUT_MAX_CODEPOINTS, if provider == Provider::Google { Some(MAX_POST_Q_BYTES) } else { None });
    Some(PreparedInput { text: c.text, truncated: c.truncated, had_spoiler })
}

/// `prepareAutoInput(content)`.
pub fn prepare_auto_input(app: &App, content: &str) -> Option<PreparedInput> {
    prepare_auto_input_for(provider_name(app), content)
}

static ECHO_STRIP: LazyRe = lazy_re!(r"[\s\p{P}\p{S}]", "u");

/// `isEcho(a, b)`: true when a "translation" only repeats the source (ignoring case, spaces and punctuation).
pub fn is_echo(a: &str, b: &str) -> bool {
    let norm = |s: &str| ECHO_STRIP.replace_all(&js::lower(s), "");
    norm(a) == norm(b)
}

// ── Translation core ──────────────────────────────────────────────────────────

static LATIN_LETTER: LazyRe = lazy_re!(r"\p{Script=Latin}", "u");
static HAN_CHAR: LazyRe = lazy_re!(r"\p{Script=Han}", "u");
static KANA_OR_HANGUL: LazyRe = lazy_re!(r"[\p{Script=Hiragana}\p{Script=Katakana}\p{Script=Hangul}]", "u");

/// `reconcileDetected(text, detected)`: rejects an impossible detection. A language written in the Latin script
/// cannot be the source of text without a single Latin letter: Han-only text is then taken as Traditional Chinese,
/// anything else is unknown.
pub fn reconcile_detected(text: &str, detected: Option<String>) -> Option<String> {
    let d = detected?;
    if LATIN_LETTER.test(text) || !LETTER.test(text) {
        return Some(d);
    }
    if !LATIN_CODES.contains(d.as_str()) {
        return Some(d);
    }
    if HAN_CHAR.test(text) && !KANA_OR_HANGUL.test(text) { Some("zh-tw".to_string()) } else { None }
}

/// The result-cache key: `tr:{provider}:{target}:{sha1(text)}`.
pub fn result_key(provider: Provider, target: &str, text: &str) -> String {
    use sha1::Digest;
    format!("tr:{}:{}:{}", provider.as_str(), target, hex::encode(sha1::Sha1::digest(text.as_bytes())))
}

/// `translateText(text, target, { source, maxChunks })`: translates `text` into `target` (a canonical LANGUAGES
/// key). The source is detected by the provider unless the caller passes a canonical `source` (`"auto"`: detect).
pub async fn translate_text(app: &App, text: &str, target: &str, source: &str, max_chunks: usize) -> Result<Translated, TranslationError> {
    if !is_language(target) || is_supported_target(app, target) == Some(false) {
        return Err(TranslationError::new(ErrorCode::UnsupportedLanguage, format!("Unsupported target \"{target}\"")).lang(target));
    }
    assert_not_paused(app).await?;
    let input = js::trim(text);
    if input.is_empty() {
        return Err(TranslationError::new(ErrorCode::Empty, "Nothing to translate"));
    }
    let cfg = config(app);
    // Fail-open: Redis being down only means a network request is made.
    let cacheable = cfg.result_cache && input.chars().count() <= RESULT_CACHE_MAX_CODEPOINTS;
    let key = cacheable.then(|| result_key(cfg.provider, target, input));
    if let Some(k) = &key
        && let Some(hit) = app.cache().get(k).await
        && let Some(t) = hit.get("t").and_then(Value::as_str)
    {
        return Ok(Translated { text: t.to_string(), detected: hit.get("d").and_then(Value::as_str).map(String::from) });
    }

    let mut via_fallback = false; // the fallback detects fewer languages: its answers are not cached
    let (translated, detected) = if cfg.provider == Provider::LibreTranslate {
        let (r, fb) = lt_translate(app, input, target, source).await?;
        via_fallback = fb;
        let d = if source == "auto" { reconcile_detected(input, r.detected) } else { r.detected };
        (r.text, d)
    } else if js::encoded_len(input) <= MAX_POST_Q_BYTES {
        let r = google_translate(app, input, target, source).await?;
        let d = if source == "auto" { reconcile_detected(input, r.detected) } else { r.detected };
        (r.text, d)
    } else {
        let pieces = split_for_requests(input, MAX_POST_Q_BYTES);
        if pieces.len() > max_chunks {
            return Err(TranslationError::new(ErrorCode::TooLong, "Text is too long to translate"));
        }
        let mut translated = String::new();
        let mut detected: Option<String> = None;
        let mut from = source.to_string();
        for piece in &pieces {
            let r = google_translate(app, &piece.text, target, &from).await?;
            translated.push_str(&r.text);
            translated.push_str(&piece.sep);
            if detected.is_none() {
                detected = if from == "auto" { reconcile_detected(&piece.text, r.detected) } else { r.detected };
                if from == "auto"
                    && let Some(d) = detected.as_ref().filter(|d| is_language(d))
                {
                    from = d.clone();
                }
            }
        }
        (js::trim(&translated).to_string(), detected)
    };

    if let Some(k) = &key
        && !via_fallback
    {
        app.cache().set(k, &json!({ "t": translated, "d": detected }), RESULT_CACHE_TTL_S).await;
    }
    Ok(Translated { text: translated, detected })
}

/// One translation of [`translate_to_targets`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct TargetText {
    pub target: String,
    pub text: String,
}

/// A target that failed, with the error code.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct TargetFailure {
    pub target: String,
    pub code: String,
}

/// The result of [`translate_to_targets`].
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize)]
pub struct TargetsOutcome {
    pub detected: Option<String>,
    pub results: Vec<TargetText>,
    pub failed: Vec<TargetFailure>,
    #[serde(rename = "pausedStop")]
    pub paused_stop: bool,
}

/// `translateToTargets(text, targets, { deadline })`: one request per target at most and no separate detect
/// request: the first response supplies the detected language. Never fails. `deadline_ms` defaults to now +
/// AUTO_DEADLINE_MS.
pub async fn translate_to_targets<S: AsRef<str>>(app: &App, text: &str, targets: &[S], deadline_ms: Option<i64>) -> TargetsOutcome {
    let deadline = deadline_ms.unwrap_or_else(|| now(app) + AUTO_DEADLINE_MS);
    let mut out = TargetsOutcome::default();
    for target in targets {
        let target = target.as_ref();
        if now(app) >= deadline {
            out.failed.push(TargetFailure { target: target.into(), code: "TIMEOUT".into() });
            continue;
        }
        if out.detected.as_deref() == Some(target) {
            continue; // same language: no request
        }
        if is_supported_target(app, target) == Some(false) {
            out.failed.push(TargetFailure { target: target.into(), code: "UNSUPPORTED_LANGUAGE".into() });
            continue;
        }
        let source = match out.detected.as_deref() {
            Some(d) if is_language(d) && is_supported_target(app, d) != Some(false) => d.to_string(),
            _ => "auto".to_string(),
        };
        match translate_text(app, text, target, &source, 1).await {
            Ok(r) => {
                if out.detected.is_none() && r.detected.is_some() {
                    out.detected = r.detected.clone(); // the first response supplies the detection
                }
                if r.detected.as_deref() == Some(target) {
                    continue; // the first target was the source language: drop it
                }
                if js::trim(&r.text).is_empty() || is_echo(text, &r.text) {
                    continue; // nothing new to show
                }
                out.results.push(TargetText { target: target.into(), text: r.text });
            }
            Err(e) => {
                if matches!(e.code, ErrorCode::Paused | ErrorCode::RateLimited) {
                    out.paused_stop = true;
                    break;
                }
                out.failed.push(TargetFailure { target: target.into(), code: e.code.as_str().into() });
            }
        }
    }
    out
}

/// The result of [`detect_language`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct DetectedLanguage {
    pub code: Option<String>,
    pub name: String,
}

/// `detectLanguage(text)`: the language of `text` (used by /translate detect). Long input is sampled.
pub async fn detect_language(app: &App, text: &str) -> Result<DetectedLanguage, TranslationError> {
    let sample = clamp_for_request(&clean_for_translation(text), DETECT_SAMPLE_CODEPOINTS, Some(MAX_Q_ENCODED_BYTES)).text;
    if !has_translatable_text(&sample) {
        return Err(TranslationError::new(ErrorCode::Empty, "Nothing to analyse"));
    }
    assert_not_paused(app).await?;
    let raw = if provider_name(app) == Provider::LibreTranslate {
        lt_detect(app, &sample).await?
    } else {
        google_translate(app, &sample, "en", "auto").await?.detected
    };
    let code = reconcile_detected(&sample, raw);
    let name = if code.is_some() { language_name(code.as_deref()) } else { "Unknown".to_string() };
    Ok(DetectedLanguage { code, name })
}

// ── Error descriptions for commands ─────────────────────────────────────────────

/// What a command shows and logs for a translation failure.
#[derive(Debug, Clone)]
pub struct DescribedError {
    /// `'warn'` or `'error'`.
    pub level: &'static str,
    /// `'none'`, `'warn'` or `'error'`.
    pub log: &'static str,
    pub embed: CreateEmbed,
}

/// `describeTranslationError(err)`; `None` is an error that is not a TranslationError.
pub fn describe_translation_error(app: &App, err: Option<&TranslationError>) -> DescribedError {
    let fail = |log, title: &str, text: &str| DescribedError { level: "error", log, embed: bot_core::embed_builder::error_embed(title, Some(text), None) };
    match err.map(|e| e.code) {
        Some(ErrorCode::Paused | ErrorCode::RateLimited) => {
            let ms = err.and_then(|e| e.retry_after_ms).unwrap_or_else(|| pause_remaining_ms(app));
            let minutes = (ms / 60_000.0).ceil().max(1.0);
            let when = bot_util::time::discord_timestamp_ms(now(app) as f64 + ms, "R");
            let text = format!(
                "The translation provider is rate-limiting this bot, so translation is paused. Try again {when} (about {} minute{}).",
                js::number_to_string(minutes),
                if minutes == 1.0 { "" } else { "s" }
            );
            DescribedError { level: "warn", log: "none", embed: bot_core::embed_builder::warn_embed("Translation Paused", Some(&text), None) }
        }
        Some(ErrorCode::UnsupportedLanguage) => {
            let which = match err.and_then(|e| e.lang.as_deref()).filter(|l| !l.is_empty()) {
                Some(l) => format!("{} (`{l}`)", language_name(Some(l))),
                None => "That language".to_string(),
            };
            let text = format!(
                "{which} isn't supported by this bot's translation backend ({}). Use `/translate languages` to see what is available.",
                provider_label(app)
            );
            fail("none", "Language Not Supported", &text)
        }
        Some(ErrorCode::Empty) => fail("none", "Nothing to Translate", "That text has no words to translate."),
        Some(ErrorCode::TooLong) => fail("warn", "Text Too Long", "That text is too long to translate in one go. Please shorten it."),
        Some(ErrorCode::Timeout) => fail("warn", "Translation Failed", "The translation service took too long to answer. Please try again."),
        Some(ErrorCode::Network | ErrorCode::HttpError | ErrorCode::BadResponse) => {
            fail("warn", "Translation Failed", "Could not reach the translation service. Please try again later.")
        }
        None => fail("error", "Translation Failed", "Something went wrong while translating. Please try again."),
    }
}

// ── Output formatting ───────────────────────────────────────────────────────────

/// `balanceMarkdown(text)`: closes markdown a cut left open (fences, **, __, ~~, backticks).
pub fn balance_markdown(text: &str) -> String {
    let mut s = text.to_string();
    let count = |s: &str, needle: &str| s.matches(needle).count();
    if count(&s, "```") % 2 == 1 {
        return format!("{s}\n```");
    }
    for marker in ["**", "__", "~~"] {
        if count(&s, marker) % 2 == 1 {
            s.push_str(marker);
        }
    }
    let singles = s.matches('`').count() as i64 - 3 * count(&s, "```") as i64;
    if singles.rem_euclid(2) == 1 {
        s.push('`');
    }
    s
}

/// `formatTargetLine(target, body)`: e.g. "🇹🇼 **ZH-TW** — ...".
pub fn format_target_line(target: &str, body: &str) -> String {
    format!("{} **{}** — {body}", lang_flag(target), target.to_uppercase())
}

/// The inputs of [`build_auto_translate_embed`].
#[derive(Debug, Clone, Default)]
pub struct AutoEmbedArgs<'a> {
    pub results: &'a [TargetText],
    pub detected: Option<&'a str>,
    pub truncated_input: bool,
    pub had_spoiler: bool,
    pub paused_stop: bool,
    pub settings: Option<&'a GuildSettings>,
}

/// `buildAutoTranslateEmbed(...)`: one line per target within a 4,000 character budget; a cut line ends with an
/// ellipsis and balanced markdown; text from a spoilered message is fully spoilered.
pub fn build_auto_translate_embed(args: &AutoEmbedArgs<'_>) -> CreateEmbed {
    let n = args.results.len().max(1) as i64;
    let per = AUTO_PER_TARGET_MAX.min(AUTO_DESCRIPTION_BUDGET / n - 24);
    let lines: Vec<String> = args
        .results
        .iter()
        .map(|r| {
            let room = per - if args.had_spoiler { 4 } else { 0 } - 8;
            let mut body = bot_util::text::clamp_text_f64(&r.text, room as f64, bot_util::text::ELLIPSIS);
            if body != r.text {
                body = balance_markdown(&body);
            }
            if args.had_spoiler {
                body = format!("||{}||", body.replace("||", ""));
            }
            format_target_line(&r.target, &body)
        })
        .collect();
    let footer = format!(
        "Detected: {}{}{}",
        language_name(args.detected),
        if args.truncated_input { " · only the first part of this long message was translated" } else { "" },
        if args.paused_stop { " · some languages were skipped (rate limited)" } else { "" }
    );
    let mut embed = bot_core::embed_builder::apply_guild_color(CreateEmbed::new().color(bot_config::colors::INFO), args.settings);
    let description = bot_util::text::clamp_text(&lines.join("\n"), 4096);
    if !description.is_empty() {
        embed = embed.description(description);
    }
    embed.footer(CreateEmbedFooter::new(bot_util::text::clamp_text(&footer, 2048)))
}

/// `buildAutoReplyPayload(embed)`: the reply never pings (the author is not pinged either), and
/// `failIfNotExists` keeps a translation from being posted for a message deleted meanwhile.
pub fn build_auto_reply_payload(embed: CreateEmbed, message: &serenity::all::Message) -> CreateMessage {
    CreateMessage::new()
        .embed(embed)
        .allowed_mentions(CreateAllowedMentions::new().replied_user(false))
        .reference_message(serenity::all::MessageReference::from(message).fail_if_not_exists(true))
}

// ── Channel configuration cache ─────────────────────────────────────────────────

/// `guild:{g}:translateChannels`.
pub fn channels_key(guild_id: &str) -> String {
    format!("guild:{guild_id}:translateChannels")
}

/// A row as the cache holds it (`{ channelId, targetLangs }`, the Prisma `select` order).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ChannelRow {
    #[serde(rename = "channelId")]
    pub channel_id: String,
    #[serde(rename = "targetLangs")]
    pub target_langs: Vec<Value>,
}

fn valid_rows(v: &Value) -> Option<Vec<ChannelRow>> {
    let arr = v.as_array()?;
    let mut out = Vec::with_capacity(arr.len());
    for r in arr {
        let channel_id = r.get("channelId")?.as_str()?.to_string();
        let target_langs = r.get("targetLangs")?.as_array()?.clone();
        out.push(ChannelRow { channel_id, target_langs });
    }
    Some(out)
}

fn to_map(rows: &[ChannelRow]) -> ChannelTargets {
    let mut map = ChannelTargets::new();
    for r in rows {
        let mut valid = normalize_target_values(&r.target_langs);
        valid.truncate(MAX_TARGETS);
        map.insert(r.channel_id.clone(), valid);
    }
    map
}

fn memoize(g: &mut Inner, guild_id: &str, map: ChannelTargets, at: i64) {
    g.channel_memo.shift_remove(guild_id);
    g.channel_memo.insert(guild_id.to_string(), (at, map));
    while g.channel_memo.len() > CHANNEL_MEMO_MAX {
        g.channel_memo.shift_remove_index(0);
    }
}

fn gen_of(g: &Inner, guild_id: &str) -> u64 {
    g.channel_gen.get(guild_id).copied().unwrap_or(0)
}

fn bump_gen(g: &mut Inner, guild_id: &str) -> u64 {
    let next = gen_of(g, guild_id) + 1;
    g.channel_gen.shift_remove(guild_id);
    g.channel_gen.insert(guild_id.to_string(), next);
    while g.channel_gen.len() > CHANNEL_MEMO_MAX {
        g.channel_gen.shift_remove_index(0);
    }
    next
}

/// `SELECT channelId, targetLangs FROM TranslateChannel WHERE guildId` (Prisma `findMany` with a select).
pub async fn load_rows(pool: &bot_db::PgPool, guild_id: &str) -> Result<Vec<ChannelRow>, DbError> {
    let rows: Vec<(String, Vec<String>)> =
        sqlx::query_as(r#"SELECT "channelId", "targetLangs" FROM "TranslateChannel" WHERE "guildId" = $1"#).bind(guild_id).fetch_all(pool).await?;
    Ok(rows
        .into_iter()
        .map(|(channel_id, langs)| ChannelRow { channel_id, target_langs: langs.into_iter().map(Value::String).collect() })
        .collect())
}

/// `getGuildTranslateConfig(guildId)`: every configured channel of a guild with its canonical target list. Reads
/// the memo, then Redis, then Postgres (a Redis miss or outage reads Postgres). Fails only when Postgres fails.
pub async fn get_guild_translate_config(app: &App, guild_id: &str) -> Result<ChannelTargets, DbError> {
    let st = inner(app);
    let generation = {
        let g = lock(&st.inner);
        if let Some((at, map)) = g.channel_memo.get(guild_id)
            && now(app) - at < CHANNELS_MEMO_TTL_MS
        {
            return Ok(map.clone());
        }
        gen_of(&g, guild_id) // a change that lands while this read is in flight must not be overwritten by it
    };
    let key = channels_key(guild_id);
    let rows = match app.cache().get(&key).await.as_ref().and_then(valid_rows) {
        Some(rows) => rows,
        None => {
            let rows = load_rows(app.db(), guild_id).await?;
            // setNX: a reader that loaded rows just before an /autotranslate change must not overwrite the new list.
            if gen_of(&lock(&st.inner), guild_id) == generation {
                app.cache().set_nx(&key, &rows, CHANNELS_CACHE_TTL_S).await;
            }
            rows
        }
    };
    let map = to_map(&rows);
    let mut g = lock(&st.inner);
    if gen_of(&g, guild_id) == generation {
        memoize(&mut g, guild_id, map.clone(), now(app));
    }
    Ok(map)
}

/// `getChannelTargets(guildId, channelId)`: the target languages of one channel, or `None`. Exact channel match only
/// (a thread does not inherit its parent's configuration). Never fails.
pub async fn get_channel_targets(app: &App, guild_id: &str, channel_id: &str) -> Option<Vec<String>> {
    match get_guild_translate_config(app, guild_id).await {
        Ok(map) => map.get(channel_id).cloned(),
        Err(e) => {
            if throttles(app).ten_min.allow(&format!("config:{guild_id}")) {
                bot_util::log_warn!(format!("translationService: could not load the channel config of guild {guild_id}: {e}"));
            }
            None
        }
    }
}

/// `refreshTranslateCache(guildId)`: write-through after a change (a plain SET that overwrites). Never fails.
pub async fn refresh_translate_cache(app: &App, guild_id: &str) {
    let st = inner(app);
    let generation = bump_gen(&mut lock(&st.inner), guild_id); // reads that began before this change no longer store
    let key = channels_key(guild_id);
    match load_rows(app.db(), guild_id).await {
        Ok(rows) => {
            if gen_of(&lock(&st.inner), guild_id) != generation {
                return; // a newer refresh superseded this one and writes the newer list
            }
            app.cache().set(&key, &rows, CHANNELS_CACHE_TTL_S).await;
            let mut g = lock(&st.inner);
            if gen_of(&g, guild_id) != generation {
                return;
            }
            bump_gen(&mut g, guild_id); // also reads that began while this refresh ran
            memoize(&mut g, guild_id, to_map(&rows), now(app));
        }
        Err(e) => {
            {
                let mut g = lock(&st.inner);
                bump_gen(&mut g, guild_id);
                g.channel_memo.shift_remove(guild_id);
            }
            app.cache().del_one(&key).await;
            bot_util::log_warn!(format!("translationService: could not refresh the channel config of guild {guild_id}: {e}"));
        }
    }
}

/// `invalidateTranslateCache` (deprecated alias of [`refresh_translate_cache`]).
pub async fn invalidate_translate_cache(app: &App, guild_id: &str) {
    refresh_translate_cache(app, guild_id).await
}

/// `__testing.primeChannelConfig(guildId, channels)`: puts a fresh entry in the channel memo (no DB or Redis).
pub fn prime_channel_config(app: &App, guild_id: &str, channels: &[(&str, &[&str])]) {
    let map: ChannelTargets = channels.iter().map(|(id, langs)| (id.to_string(), langs.iter().map(|l| l.to_string()).collect())).collect();
    let st = inner(app);
    memoize(&mut lock(&st.inner), guild_id, map, now(app));
}

// ── Capped writes ─────────────────────────────────────────────────────────────

/// The advisory-lock key of a guild's translated channels (shared with the dashboard).
pub fn channels_lock_key(guild_id: &str) -> String {
    format!("translate-channels:{guild_id}")
}

/// A raw statement and its bind values (the JS `RAW_SQL` entries), for the database tests.
#[derive(Debug, Clone, PartialEq)]
pub struct RawSql {
    pub name: &'static str,
    pub sql: String,
    pub values: Vec<Value>,
}

/// `RAW_SQL`: the raw statements of this service.
pub fn raw_sql() -> Vec<RawSql> {
    vec![RawSql { name: "translation.channelsLock", sql: bot_db::tx::ADVISORY_XACT_LOCK_SQL.to_string(), values: vec![json!(channels_lock_key("0"))] }]
}

/// The result of [`save_translate_channel`].
#[derive(Debug, Clone, PartialEq)]
pub enum SaveOutcome {
    /// `{ ok: true, created, row }`.
    Saved { created: bool, row: TranslateChannel },
    /// `{ ok: false, code: 'LIMIT', count }`.
    Limit { count: i64 },
    /// `{ ok: false, code: 'EXISTS' }`.
    Exists,
}

/// `saveTranslateChannel(guildId, channelId, targetLangs, { updateExisting })`: adds a translated channel, or sets
/// the languages of an existing one, under LIMITS.TRANSLATE_CHANNELS. The per-guild advisory lock makes the cap exact
/// when /autotranslate and the dashboard save at the same time. The caller does `ensure_guild_settings` first and
/// [`refresh_translate_cache`] after.
pub async fn save_translate_channel(
    app: &App,
    guild_id: &str,
    channel_id: &str,
    target_langs: &[String],
    update_existing: bool,
) -> Result<SaveOutcome, DbError> {
    let (g, c, langs) = (guild_id.to_string(), channel_id.to_string(), target_langs.to_vec());
    let opts = bot_db::TxOptions { timeout: std::time::Duration::from_secs(10), max_wait: std::time::Duration::from_secs(10) };
    bot_db::transaction(app.db(), opts, move |tx| {
        Box::pin(async move {
            bot_db::advisory_xact_lock(tx, &channels_lock_key(&g)).await?;
            let existing: Option<(String,)> = sqlx::query_as(r#"SELECT "id" FROM "TranslateChannel" WHERE "guildId" = $1 AND "channelId" = $2"#)
                .bind(&g)
                .bind(&c)
                .fetch_optional(&mut *tx)
                .await?;
            if existing.is_some() && !update_existing {
                return Ok(SaveOutcome::Exists);
            }
            if existing.is_none() {
                let (count,): (i64,) =
                    sqlx::query_as(r#"SELECT COUNT(*) FROM "TranslateChannel" WHERE "guildId" = $1"#).bind(&g).fetch_one(&mut *tx).await?;
                if count >= bot_config::tables::limits::TRANSLATE_CHANNELS as i64 {
                    return Ok(SaveOutcome::Limit { count });
                }
            }
            let row: TranslateChannel = bot_db::InsertBuilder::new(&TRANSLATE_CHANNEL_INFO)
                .values([("guildId", json!(g)), ("channelId", json!(c)), ("targetLangs", json!(langs))])?
                .on_conflict_update(&["guildId", "channelId"], [("targetLangs", json!(langs))])?
                .build()?
                .fetch_one(&mut *tx)
                .await?;
            Ok(SaveOutcome::Saved { created: existing.is_none(), row })
        })
    })
    .await
}

// ── The auto-translate hook body ──────────────────────────────────────────────

const REPLY_GONE: [u64; 2] = [10008, 50035];
const REPLY_BLOCKED: [u64; 5] = [10003, 50001, 50013, 50083, 160005];
/// What the bot needs in a channel to post translations (replies need Read Message History).
pub const AUTO_NEEDS: [&str; 4] = ["ViewChannel", "SendMessages", "EmbedLinks", "ReadMessageHistory"];

/// The message fields `handleAutoTranslate` reads.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AutoMessage {
    pub id: String,
    pub channel_id: String,
    pub content: String,
    /// discord.js `message.system`.
    pub system: bool,
    pub author_bot: bool,
    pub webhook_id: Option<String>,
}

impl AutoMessage {
    /// From a gateway message.
    pub fn of(message: &serenity::all::Message) -> Self {
        use serenity::all::MessageType as T;
        Self {
            id: message.id.to_string(),
            channel_id: message.channel_id.to_string(),
            content: message.content.clone(),
            system: !matches!(message.kind, T::Regular | T::InlineReply | T::ChatInputCommand | T::ContextMenuCommand),
            author_bot: message.author.bot,
            webhook_id: message.webhook_id.map(|w| w.to_string()),
        }
    }
}

/// A failed reply: the Discord error code and message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplyError {
    pub code: Option<u64>,
    pub message: String,
}

impl ReplyError {
    pub fn of(err: &serenity::Error) -> Self {
        Self { code: bot_core::discord::error_code(err), message: bot_core::discord::error_message(err) }
    }
}

/// The context of one [`handle_auto_translate`] run.
pub struct AutoCtx<'a> {
    pub guild_id: &'a str,
    pub message: &'a AutoMessage,
    /// `ctx.state.automodAction?.deleted`.
    pub automod_deleted: bool,
    /// `ctx.getSettings()`, awaited only when there is something to post.
    pub settings: BoxFuture<'static, Option<bot_core::settings::Settings>>,
    /// The JS `checkPermissions` option (false only in offline tests).
    pub check_permissions: bool,
}

struct InflightGuard(Arc<TranslationState>);

impl Drop for InflightGuard {
    fn drop(&mut self) {
        let mut g = lock(&self.0.inner);
        g.auto_inflight = g.auto_inflight.saturating_sub(1);
    }
}

/// `handleAutoTranslate(ctx)`: translates a message into the targets of its channel and replies with one embed
/// (`reply` sends it: the gateway message's reply in production, a recorder in tests). Never fails.
pub async fn handle_auto_translate<R, RF>(app: &App, ctx: AutoCtx<'_>, reply: R)
where
    R: FnOnce(CreateEmbed) -> RF,
    RF: Future<Output = Result<(), ReplyError>>,
{
    let AutoCtx { guild_id, message, automod_deleted, settings, check_permissions } = ctx;
    if guild_id.is_empty() {
        return;
    }
    if message.system || message.author_bot || message.webhook_id.is_some() {
        return;
    }
    if automod_deleted || bot_core::deletion_tracker::was_deleted_by_bot(app, &message.id).is_some() {
        return;
    }
    // Everything below runs only in configured channels, so other channels never run the cleaner.
    let Some(targets) = get_channel_targets(app, guild_id, &message.channel_id).await.filter(|t| !t.is_empty()) else { return };
    if is_link_only(&message.content) {
        return;
    }
    let Some(prepared) = prepare_auto_input(app, &message.content) else { return };

    let targets: Vec<String> = targets
        .into_iter()
        .filter(|code| {
            if is_supported_target(app, code) != Some(false) {
                return true;
            }
            if throttles(app).hour.allow(&format!("unsupported:{guild_id}:{code}")) {
                bot_util::log_warn!(format!(
                    "Auto-translate target \"{code}\" in guild {guild_id} is not supported by {}; remove it with /autotranslate.",
                    provider_label(app)
                ));
            }
            false
        })
        .collect();
    if targets.is_empty() {
        return;
    }

    if check_permissions {
        let gid = guild_id.parse::<u64>().ok().filter(|v| *v != 0).map(serenity::all::GuildId::new);
        let cid = message.channel_id.parse::<u64>().ok().filter(|v| *v != 0).map(serenity::all::ChannelId::new);
        let usable = match gid {
            Some(g) => bot_core::interaction_helpers::check_channel_usable(app, g, cid, &AUTO_NEEDS),
            None => bot_core::interaction_helpers::ChannelUsable { ok: false, missing: Vec::new(), reason: None },
        };
        if !usable.ok {
            if throttles(app).ten_min.allow(&format!("perm:{}", message.channel_id)) {
                let missing = if usable.missing.is_empty() { "access".to_string() } else { usable.missing.join(", ") };
                bot_util::log_warn!(format!("Auto-translate: missing {missing} in {}", message.channel_id));
            }
            return; // before any request: no wasted quota
        }
    }

    let st = inner(app);
    {
        let mut g = lock(&st.inner);
        if g.auto_inflight >= AUTO_MAX_INFLIGHT {
            drop(g);
            if throttles(app).minute.allow("busy") {
                bot_util::log_warn!("Auto-translate is busy; skipping messages");
            }
            return;
        }
        g.auto_inflight += 1;
    }
    let _guard = InflightGuard(st);

    let outcome = translate_to_targets(app, &prepared.text, &targets, Some(now(app) + AUTO_DEADLINE_MS)).await;
    let mut codes: Vec<&str> = Vec::new();
    for f in outcome.failed.iter().filter(|f| f.code != "UNSUPPORTED_LANGUAGE") {
        if !codes.contains(&f.code.as_str()) {
            codes.push(&f.code);
        }
    }
    if !codes.is_empty() && throttles(app).ten_min.allow(&format!("fail:{}", message.channel_id)) {
        bot_util::log_warn!(format!("Auto-translate failed in {}: {}", message.channel_id, codes.join(", ")));
    }
    if outcome.results.is_empty() {
        return;
    }
    if bot_core::deletion_tracker::was_deleted_by_bot(app, &message.id).is_some() {
        return;
    }

    let settings = settings.await;
    let embed = build_auto_translate_embed(&AutoEmbedArgs {
        results: &outcome.results,
        detected: outcome.detected.as_deref(),
        truncated_input: prepared.truncated,
        had_spoiler: prepared.had_spoiler,
        paused_stop: outcome.paused_stop,
        settings: settings.as_deref(),
    });
    match reply(embed).await {
        Ok(()) => {
            // Unawaited: counting never delays or fails the hook.
            let (a, g, c) = (app.clone(), guild_id.to_string(), message.channel_id.clone());
            let detected = outcome.detected.clone();
            let targets: Vec<String> = outcome.results.iter().map(|r| r.target.clone()).collect();
            tokio::spawn(async move {
                stats::record_translation(&a, &g, &c, StatVia::Auto, detected.as_deref(), &targets).await;
            });
        }
        Err(e) => match e.code {
            Some(code) if REPLY_GONE.contains(&code) => {
                bot_util::log_debug!(format!("Auto-translate: the message {} is gone ({code})", message.id));
            }
            Some(code) if REPLY_BLOCKED.contains(&code) => {
                if throttles(app).ten_min.allow(&format!("reply:{}", message.channel_id)) {
                    bot_util::log_warn!(format!("Auto-translate cannot reply in {} ({code})", message.channel_id));
                }
            }
            _ => bot_util::log_warn!(format!("Auto-translate reply failed in {}: {}", message.channel_id, e.message)),
        },
    }
}

/// The number of auto-translations running now (`autoInflight`).
pub fn auto_inflight(app: &App) -> usize {
    lock(&inner(app).inner).auto_inflight
}
