//! Port of `src/dashboard/config.js`: every environment variable the dashboard reads is parsed and validated here.
//! Pure: nothing is read until [`load_dashboard_config`] (or [`load_dashboard_config_from_env`]) is called.

use std::collections::HashMap;
use std::sync::LazyLock;

use bot_util::js::{trim, utf16_len};
use bot_util::jsre::JsRegex;
use serde::{Deserialize, Serialize};

use crate::js_ip::is_ip;

/// `DASHBOARD_DEFAULTS.url`.
pub const DEFAULT_URL: &str = "http://localhost:8080";
/// `DASHBOARD_DEFAULTS.port`.
pub const DEFAULT_PORT: u16 = 8080;
/// `DASHBOARD_DEFAULTS.host`.
pub const DEFAULT_HOST: &str = "0.0.0.0";

/// Keys the dashboard cannot run without (the bot itself needs only the last four).
pub const REQUIRED_KEYS: [&str; 6] =
    ["CLIENT_ID", "DISCORD_TOKEN", "DISCORD_CLIENT_SECRET", "DASHBOARD_SESSION_SECRET", "DATABASE_URL", "REDIS_URL"];

/// `MIN_SESSION_SECRET_LENGTH`.
pub const MIN_SESSION_SECRET_LENGTH: usize = 32;

static SNOWFLAKE_RE: LazyLock<JsRegex> = LazyLock::new(|| JsRegex::new(r"^\d{17,20}$", ""));
static EMAIL_RE: LazyLock<JsRegex> =
    LazyLock::new(|| JsRegex::new(r"^[A-Za-z0-9._%+-]+@[A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)+$", ""));
static PORT_RE: LazyLock<JsRegex> = LazyLock::new(|| JsRegex::new(r"^\d{1,5}$", ""));
static TRAILING_SLASHES_RE: LazyLock<JsRegex> = LazyLock::new(|| JsRegex::new(r"/+$", ""));

/// Whether a string is a Discord snowflake as the dashboard checks it (`/^\d{17,20}$/`).
pub fn is_snowflake(s: &str) -> bool {
    SNOWFLAKE_RE.test(s)
}

/// An IP address in the form `req.socket.remoteAddress` reports it: lower case, IPv4-mapped IPv6 unwrapped.
pub fn normalize_ip(value: &str) -> String {
    let ip = trim(value).to_lowercase();
    match ip.strip_prefix("::ffff:") {
        Some(rest) if is_ip(rest) == 4 => rest.to_string(),
        _ => ip,
    }
}

/// The contact shown on the privacy and terms pages.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Contact {
    /// `"email"` or `"url"`.
    pub kind: ContactKind,
    pub value: String,
}

/// `contact.kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ContactKind {
    Email,
    Url,
}

/// WHATWG `new URL(raw)` and its `.search`/`.hash` as JS reports them (an empty `?` or `#` reads as `''`).
fn parse_url(raw: &str) -> Option<url::Url> {
    url::Url::parse(raw).ok()
}

fn js_search(u: &url::Url) -> &str {
    u.query().unwrap_or("")
}

fn js_hash(u: &url::Url) -> &str {
    u.fragment().unwrap_or("")
}

/// Parses `DASHBOARD_CONTACT` (and `DASHBOARD_SUPPORT_URL`): an email address or an https URL; `None` when neither.
pub fn parse_contact(raw: &str) -> Option<Contact> {
    if utf16_len(raw) > 254 {
        return None;
    }
    if EMAIL_RE.test(raw) {
        return Some(Contact { kind: ContactKind::Email, value: raw.to_string() });
    }
    let url = parse_url(raw)?;
    if url.scheme() == "https" && url.username().is_empty() && url.password().unwrap_or("").is_empty() {
        return Some(Contact { kind: ContactKind::Url, value: url.as_str().to_string() });
    }
    None
}

/// Parses `DASHBOARD_URL` into its public origin. Only a bare origin is accepted.
/// `Ok(origin)` or `Err(reason)` with the JS text.
pub fn parse_dashboard_url(raw: &str) -> Result<String, &'static str> {
    let Some(url) = parse_url(raw) else {
        return Err("must be an absolute http(s) URL such as https://dash.example.com");
    };
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err("must use http or https");
    }
    if !url.username().is_empty() || !url.password().unwrap_or("").is_empty() {
        return Err("must not contain credentials");
    }
    if url.path() != "/" || !js_search(&url).is_empty() || !js_hash(&url).is_empty() {
        return Err("must be the bare origin (no path, query or fragment), e.g. https://dash.example.com");
    }
    Ok(url.origin().ascii_serialization())
}

/// The validated dashboard configuration (`config` of `loadDashboardConfig`). Serialises to the JS object's JSON.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DashboardConfig {
    pub client_id: String,
    pub bot_token: String,
    pub client_secret: String,
    pub session_secret: String,
    pub database_url: String,
    pub redis_url: String,
    pub bot_owner_id: Option<String>,
    pub url: String,
    pub origin: String,
    pub url_is_default: bool,
    pub redirect_uri: String,
    pub secure_cookies: bool,
    pub port: u16,
    pub host: String,
    pub trusted_proxies: Vec<String>,
    pub contact: Option<Contact>,
    pub support_url: Option<String>,
}

/// One invalid key and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InvalidKey {
    pub key: String,
    pub reason: String,
}

/// The result of `loadDashboardConfig`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoadedConfig {
    pub ok: bool,
    pub missing: Vec<String>,
    pub invalid: Vec<InvalidKey>,
    pub warnings: Vec<String>,
    pub config: DashboardConfig,
}

/// Reads and validates the dashboard configuration from a lookup function (`None` = unset). Never fails.
/// `config` is always filled (best effort, defaults for anything invalid) so a parked server can still listen on the
/// configured port; it is only safe to serve the dashboard when `ok` is true.
pub fn load_dashboard_config<F>(env: F) -> LoadedConfig
where
    F: Fn(&str) -> Option<String>,
{
    let read = |key: &str| env(key).map(|v| trim(&v).to_string()).unwrap_or_default();
    let missing: Vec<String> = REQUIRED_KEYS.iter().filter(|k| read(k).is_empty()).map(|k| k.to_string()).collect();
    let mut invalid = Vec::new();
    let mut warnings = Vec::new();
    let mut bad = |key: &str, reason: String| invalid.push(InvalidKey { key: key.to_string(), reason });

    let client_id = read("CLIENT_ID");
    if !client_id.is_empty() && !is_snowflake(&client_id) {
        bad("CLIENT_ID", "must be the application id (17-20 digits)".into());
    }

    let session_secret = read("DASHBOARD_SESSION_SECRET");
    if !session_secret.is_empty() && utf16_len(&session_secret) < MIN_SESSION_SECRET_LENGTH {
        bad("DASHBOARD_SESSION_SECRET", format!("must be at least {MIN_SESSION_SECRET_LENGTH} characters"));
    }

    let mut url = DEFAULT_URL.to_string();
    let raw_url = read("DASHBOARD_URL");
    if !raw_url.is_empty() {
        match parse_dashboard_url(&TRAILING_SLASHES_RE.replace_all(&raw_url, "")) {
            Ok(u) => url = u,
            Err(reason) => bad("DASHBOARD_URL", reason.into()),
        }
    }

    let mut port = DEFAULT_PORT;
    let raw_port = read("DASHBOARD_PORT");
    if !raw_port.is_empty() {
        let n: u32 = if PORT_RE.test(&raw_port) { raw_port.parse().unwrap_or(0) } else { 0 };
        if (1..=65_535).contains(&n) {
            port = n as u16;
        } else {
            bad("DASHBOARD_PORT", "must be a port number from 1 to 65535".into());
        }
    }

    let host = Some(read("DASHBOARD_HOST")).filter(|h| !h.is_empty()).unwrap_or_else(|| DEFAULT_HOST.to_string());

    let mut bot_owner_id = Some(read("BOT_OWNER_ID")).filter(|s| !s.is_empty());
    if bot_owner_id.as_deref().is_some_and(|id| !is_snowflake(id)) {
        warnings.push("BOT_OWNER_ID is not a Discord user id (17-20 digits); it is ignored by the dashboard.".to_string());
        bot_owner_id = None;
    }

    // Reverse proxies whose X-Forwarded-For is believed (the login rate limit's client address); loopback always is.
    let mut trusted_proxies: Vec<String> = Vec::new();
    let raw_proxies = read("DASHBOARD_TRUSTED_PROXIES");
    if !raw_proxies.is_empty() {
        let parts: Vec<&str> = raw_proxies.split(',').map(trim).filter(|s| !s.is_empty()).collect();
        if parts.iter().all(|p| is_ip(p) != 0) {
            for p in parts {
                let n = normalize_ip(p);
                if !trusted_proxies.contains(&n) {
                    trusted_proxies.push(n);
                }
            }
        } else {
            bad("DASHBOARD_TRUSTED_PROXIES", "must be a comma-separated list of IP addresses".into());
        }
    }

    let mut contact = None;
    let raw_contact = read("DASHBOARD_CONTACT");
    if !raw_contact.is_empty() {
        contact = parse_contact(&raw_contact);
        if contact.is_none() {
            warnings.push(
                "DASHBOARD_CONTACT is not an email address or an https URL; the privacy and terms pages show no contact."
                    .to_string(),
            );
        }
    }

    // The support server invite on the public pages; the pages show no support server link without it.
    let mut support_url = None;
    let raw_support = read("DASHBOARD_SUPPORT_URL");
    if !raw_support.is_empty() {
        match parse_contact(&raw_support) {
            Some(Contact { kind: ContactKind::Url, value }) => support_url = Some(value),
            _ => warnings
                .push("DASHBOARD_SUPPORT_URL is not an https URL; the public pages show no support server link.".to_string()),
        }
    }

    let origin = parse_url(&url).map(|u| u.origin().ascii_serialization()).unwrap_or_else(|| url.clone());
    let config = DashboardConfig {
        client_id,
        bot_token: read("DISCORD_TOKEN"),
        client_secret: read("DISCORD_CLIENT_SECRET"),
        session_secret,
        database_url: read("DATABASE_URL"),
        redis_url: read("REDIS_URL"),
        bot_owner_id,
        redirect_uri: format!("{url}/auth/callback"),
        secure_cookies: url.starts_with("https:"),
        url_is_default: raw_url.is_empty(),
        origin,
        url,
        port,
        host,
        trusted_proxies,
        contact,
        support_url,
    };
    let ok = missing.is_empty() && invalid.is_empty();
    LoadedConfig { ok, missing, invalid, warnings, config }
}

/// [`load_dashboard_config`] over a map of variables.
pub fn load_dashboard_config_from_map(env: &HashMap<String, String>) -> LoadedConfig {
    load_dashboard_config(|k| env.get(k).cloned())
}

/// [`load_dashboard_config`] over the process environment (`process.env`); a value that is not valid Unicode counts
/// as unset, as a non-string does in the JS.
pub fn load_dashboard_config_from_env() -> LoadedConfig {
    load_dashboard_config(|k| std::env::var(k).ok())
}
