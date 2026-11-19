//! Port of `src/dashboard/auth.js`: Discord OAuth2 (authorization code), the encrypted session cookie, the OAuth
//! `state` cookie and the CSRF checks for writes. Pure helpers plus two outbound calls ([`OAuthClient`]).
//!
//! The sealed session is byte-compatible with the JS: HKDF-SHA256 key (salt `discord-bot-dashboard`, info
//! `session-cookie/v1`), AES-256-GCM with a random 12-byte IV and AAD `v1`, written as `v1.<base64url(iv|ct|tag)>`.

use std::future::Future;
use std::time::Duration;

use base64::Engine as _;
use bot_util::js::{encode_uri_component, number_to_string, trim, utf16_len};
use bot_util::jsre::JsRegex;
use indexmap::IndexMap;
use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM};
use ring::rand::{SecureRandom, SystemRandom};
use serde_json::{Map, Value};
use std::sync::LazyLock;

use crate::config::DashboardConfig;

pub const SESSION_COOKIE: &str = "dash_session";
pub const STATE_COOKIE: &str = "dash_oauth_state";
pub const SESSION_MAX_AGE_SECONDS: u64 = 7 * 24 * 60 * 60;
pub const STATE_MAX_AGE_SECONDS: u64 = 10 * 60;
pub const OAUTH_SCOPES: [&str; 2] = ["identify", "guilds"];
pub const OAUTH_TIMEOUT_MS: u64 = 10_000;

/// `OAuth2Routes.tokenURL` of discord.js.
pub const TOKEN_URL: &str = "https://discord.com/api/v10/oauth2/token";
/// `OAuth2Routes.tokenRevocationURL` of discord.js.
pub const TOKEN_REVOCATION_URL: &str = "https://discord.com/api/v10/oauth2/token/revoke";
/// The authorize page `buildAuthorizeUrl` links to.
pub const AUTHORIZE_URL: &str = "https://discord.com/oauth2/authorize";

const SEAL_VERSION: &str = "v1";
const IV_BYTES: usize = 12;
const TAG_BYTES: usize = 16;
const MAX_SEALED_LENGTH: usize = 4096;
const HKDF_SALT: &str = "discord-bot-dashboard";
const HKDF_INFO: &str = "session-cookie/v1";
const ADMINISTRATOR: u128 = 0x8;
const MANAGE_GUILD: u128 = 0x20;

// ── Session sealing ───────────────────────────────────────────────────────────

/// The 32-byte AES key for session cookies (`deriveSessionKey`'s Buffer). Its `Debug` never shows the bytes.
#[derive(Clone, PartialEq, Eq)]
pub struct SessionKey([u8; 32]);

impl std::fmt::Debug for SessionKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SessionKey(..)")
    }
}

impl SessionKey {
    /// The raw key bytes.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// A key from raw bytes (tests, or a key derived elsewhere).
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    fn aead(&self) -> LessSafeKey {
        LessSafeKey::new(UnboundKey::new(&AES_256_GCM, &self.0).expect("a 32-byte AES-256 key"))
    }
}

struct Len32;

impl ring::hkdf::KeyType for Len32 {
    fn len(&self) -> usize {
        32
    }
}

/// Derives the AES key for session cookies from `DASHBOARD_SESSION_SECRET` (HKDF-SHA256 over its UTF-8 bytes).
/// An empty secret is an error, with the JS `TypeError` text.
pub fn derive_session_key(secret: &str) -> Result<SessionKey, &'static str> {
    if secret.is_empty() {
        return Err("deriveSessionKey needs a secret");
    }
    let salt = ring::hkdf::Salt::new(ring::hkdf::HKDF_SHA256, HKDF_SALT.as_bytes());
    let prk = salt.extract(secret.as_bytes());
    let info = [HKDF_INFO.as_bytes()];
    let mut out = [0u8; 32];
    prk.expand(&info, Len32).and_then(|okm| okm.fill(&mut out)).map_err(|_| "deriveSessionKey failed")?;
    Ok(SessionKey(out))
}

fn random_bytes<const N: usize>() -> [u8; N] {
    let mut out = [0u8; N];
    SystemRandom::new().fill(&mut out).expect("the system random source");
    out
}

/// Encrypts and authenticates a JSON payload (AES-256-GCM, random 12-byte IV) into `v1.<base64url>`.
/// The payload must carry a numeric `exp` (ms since epoch) to open again; write it as an integer.
pub fn seal_session(payload: &Value, key: &SessionKey) -> String {
    let iv: [u8; IV_BYTES] = random_bytes();
    seal_with_iv(payload, key, iv)
}

fn seal_with_iv(payload: &Value, key: &SessionKey, iv: [u8; IV_BYTES]) -> String {
    let mut in_out = serde_json::to_vec(payload).expect("a JSON value serialises");
    key.aead()
        .seal_in_place_append_tag(Nonce::assume_unique_for_key(iv), Aad::from(SEAL_VERSION.as_bytes()), &mut in_out)
        .expect("AES-GCM seal");
    let mut raw = Vec::with_capacity(IV_BYTES + in_out.len());
    raw.extend_from_slice(&iv);
    raw.extend_from_slice(&in_out);
    format!("{SEAL_VERSION}.{}", base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw))
}

/// Node's `Buffer.from(s, 'base64url')` for a string of `[A-Za-z0-9_-]`: 6-bit groups into bytes, leftover bits dropped
/// (a lone trailing character gives nothing).
fn node_base64url_decode(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut acc: u32 = 0;
    let mut bits = 0;
    for b in s.bytes() {
        let v = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'-' | b'+' => 62,
            b'_' | b'/' => 63,
            _ => continue,
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    out
}

static SEALED_CHARS_RE: LazyLock<JsRegex> = LazyLock::new(|| JsRegex::new(r"^[A-Za-z0-9_-]+$", ""));

/// Reverses [`seal_session`]. Never fails: a wrong key, a tampered or truncated value, an unknown version, malformed
/// JSON, a payload that is not an object or an `exp` that is not a finite number after `now_ms` all give `None`.
pub fn unseal_session(token: Option<&str>, key: &SessionKey, now_ms: f64) -> Option<Map<String, Value>> {
    let token = token?;
    if utf16_len(token) > MAX_SEALED_LENGTH {
        return None;
    }
    let encoded = token.strip_prefix("v1.")?;
    if !SEALED_CHARS_RE.test(encoded) {
        return None;
    }
    let raw = node_base64url_decode(encoded);
    if raw.len() < IV_BYTES + TAG_BYTES + 2 {
        return None;
    }
    let iv: [u8; IV_BYTES] = raw[..IV_BYTES].try_into().ok()?;
    let mut in_out = raw[IV_BYTES..].to_vec();
    let plain = key
        .aead()
        .open_in_place(Nonce::assume_unique_for_key(iv), Aad::from(SEAL_VERSION.as_bytes()), &mut in_out)
        .ok()?;
    let json = String::from_utf8_lossy(plain);
    let Value::Object(payload) = serde_json::from_str::<Value>(&json).ok()? else { return None };
    let exp = payload.get("exp")?.as_f64()?;
    if !exp.is_finite() || exp <= now_ms {
        return None;
    }
    Some(payload)
}

// ── Cookies ───────────────────────────────────────────────────────────────────

/// JS `decodeURIComponent`: `None` where it throws (a broken `%XX` or bytes that are not UTF-8). `+` stays `+`.
pub fn decode_uri_component(s: &str) -> Option<String> {
    if !s.contains('%') {
        return Some(s.to_string());
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = s.get(i + 1..i + 3)?;
            if !hex.bytes().all(|c| c.is_ascii_hexdigit()) {
                return None;
            }
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Parses a Cookie header. The first occurrence of a name wins; malformed pairs are skipped.
pub fn parse_cookies(header: Option<&str>) -> IndexMap<String, String> {
    let mut out = IndexMap::new();
    let Some(header) = header.filter(|h| !h.is_empty()) else { return out };
    for part in header.split(';') {
        let Some(eq) = part.find('=') else { continue };
        if eq == 0 {
            continue;
        }
        let name = trim(&part[..eq]);
        if name.is_empty() || out.contains_key(name) {
            continue;
        }
        let mut value = trim(&part[eq + 1..]);
        if value.len() >= 2 && value.starts_with('"') && value.ends_with('"') {
            value = &value[1..value.len() - 1];
        }
        // Not valid percent-encoding: ignore this cookie.
        if let Some(decoded) = decode_uri_component(value) {
            out.insert(name.to_string(), decoded);
        }
    }
    out
}

/// Options of [`serialize_cookie`] (`{ maxAge, path = '/', secure = false }`).
#[derive(Debug, Clone, PartialEq)]
pub struct CookieOptions {
    /// Seconds; `None` for a session cookie.
    pub max_age: Option<f64>,
    pub path: String,
    pub secure: bool,
}

impl Default for CookieOptions {
    fn default() -> Self {
        Self { max_age: None, path: "/".to_string(), secure: false }
    }
}

/// Builds a Set-Cookie value. HttpOnly and SameSite=Lax are always set.
pub fn serialize_cookie(name: &str, value: &str, opts: &CookieOptions) -> String {
    let mut parts = vec![
        format!("{name}={}", encode_uri_component(value)),
        format!("Path={}", opts.path),
        "HttpOnly".to_string(),
        "SameSite=Lax".to_string(),
    ];
    if let Some(max_age) = opts.max_age {
        // Math.max(0, Math.floor(maxAge)): NaN stays NaN.
        let floored = max_age.floor();
        let seconds = if floored.is_nan() { f64::NAN } else { floored.max(0.0) };
        parts.push(format!("Max-Age={}", number_to_string(seconds)));
        if seconds == 0.0 {
            parts.push("Expires=Thu, 01 Jan 1970 00:00:00 GMT".to_string());
        }
    }
    if opts.secure {
        parts.push("Secure".to_string());
    }
    parts.join("; ")
}

/// A Set-Cookie value that deletes a cookie.
pub fn clear_cookie(name: &str, path: &str, secure: bool) -> String {
    serialize_cookie(name, "", &CookieOptions { max_age: Some(0.0), path: path.to_string(), secure })
}

// ── OAuth2 ────────────────────────────────────────────────────────────────────

/// A fresh random OAuth `state` (32 bytes, base64url).
pub fn create_oauth_state() -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random_bytes::<32>())
}

/// Constant-time comparison of the `state` query parameter with the cookie value. `None` stands for a value that is
/// not a string (no cookie, no parameter).
pub fn states_match(expected: Option<&str>, actual: Option<&str>) -> bool {
    let (Some(a), Some(b)) = (expected, actual) else { return false };
    if a.is_empty() || a.len() != b.len() {
        return false;
    }
    a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// `URLSearchParams(pairs).toString()`.
fn form_body(pairs: &[(&str, &str)]) -> String {
    let mut s = url::form_urlencoded::Serializer::new(String::new());
    for (k, v) in pairs {
        s.append_pair(k, v);
    }
    s.finish()
}

/// The Discord authorize URL for a login (scopes `identify guilds`, consent skipped when already given).
pub fn build_authorize_url(client_id: &str, redirect_uri: &str, state: &str) -> String {
    let scope = OAUTH_SCOPES.join(" ");
    let query = form_body(&[
        ("response_type", "code"),
        ("client_id", client_id),
        ("scope", &scope),
        ("redirect_uri", redirect_uri),
        ("state", state),
        ("prompt", "none"),
    ]);
    // `url.search = query` leaves a URLSearchParams string as it is.
    format!("{AUTHORIZE_URL}?{query}")
}

/// What the OAuth calls need from the configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthCredentials {
    pub client_id: String,
    pub client_secret: String,
    pub redirect_uri: String,
}

impl From<&DashboardConfig> for OAuthCredentials {
    fn from(c: &DashboardConfig) -> Self {
        Self { client_id: c.client_id.clone(), client_secret: c.client_secret.clone(), redirect_uri: c.redirect_uri.clone() }
    }
}

/// The `Authorization: Basic ...` value of the token calls.
pub fn basic_auth(creds: &OAuthCredentials) -> String {
    let pair = format!("{}:{}", encode_uri_component(&creds.client_id), encode_uri_component(&creds.client_secret));
    format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(pair))
}

/// One form POST to Discord's token endpoints (`postForm`): the URL, the Basic credentials and the form body. The
/// other headers are always `Content-Type: application/x-www-form-urlencoded` and `Accept: application/json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthRequest {
    pub url: String,
    pub authorization: String,
    pub body: String,
}

/// The token exchange request for an authorization code.
pub fn token_request(creds: &OAuthCredentials, token_url: &str, code: &str) -> OAuthRequest {
    OAuthRequest {
        url: token_url.to_string(),
        authorization: basic_auth(creds),
        body: form_body(&[("grant_type", "authorization_code"), ("code", code), ("redirect_uri", &creds.redirect_uri)]),
    }
}

/// The token revocation request.
pub fn revoke_request(creds: &OAuthCredentials, revoke_url: &str, access_token: &str) -> OAuthRequest {
    OAuthRequest {
        url: revoke_url.to_string(),
        authorization: basic_auth(creds),
        body: form_body(&[("token", access_token), ("token_type_hint", "access_token")]),
    }
}

/// A failed OAuth call. `status` is set for HTTP failures; the message never contains a token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthError {
    pub message: String,
    pub status: Option<u16>,
}

impl std::fmt::Display for OAuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for OAuthError {}

impl OAuthError {
    fn plain(message: &str) -> Self {
        Self { message: message.to_string(), status: None }
    }
}

/// A granted access token. `expires_in` is in seconds.
#[derive(Debug, Clone, PartialEq)]
pub struct TokenGrant {
    pub access_token: String,
    pub expires_in: f64,
}

/// JS `String(value)` for a JSON value.
fn js_string(v: &Value) -> String {
    match v {
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => number_to_string(n.as_f64().unwrap_or(f64::NAN)),
        Value::String(s) => s.clone(),
        Value::Array(a) => a
            .iter()
            .map(|x| if x.is_null() { String::new() } else { js_string(x) })
            .collect::<Vec<_>>()
            .join(","),
        Value::Object(_) => "[object Object]".into(),
    }
}

/// JS `Number(string)`.
fn js_string_to_number(s: &str) -> f64 {
    let t = trim(s);
    if t.is_empty() {
        return 0.0;
    }
    let radix = |p: &str, r: u32| t.strip_prefix(p).or_else(|| t.strip_prefix(&p.to_uppercase())).map(|d| (d, r));
    if let Some((digits, r)) = radix("0x", 16).or_else(|| radix("0o", 8)).or_else(|| radix("0b", 2)) {
        if digits.is_empty() || !digits.chars().all(|c| c.is_digit(r)) {
            return f64::NAN;
        }
        return digits.chars().fold(0.0, |acc, c| acc * f64::from(r) + f64::from(c.to_digit(r).unwrap_or(0)));
    }
    match t {
        "Infinity" | "+Infinity" => return f64::INFINITY,
        "-Infinity" => return f64::NEG_INFINITY,
        _ => {}
    }
    let ok = t.chars().all(|c| c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '+' | '-'));
    if !ok {
        return f64::NAN;
    }
    t.parse::<f64>().unwrap_or(f64::NAN)
}

/// JS `Number(value)` for a JSON value (absent = `undefined` = NaN).
fn js_number(v: Option<&Value>) -> f64 {
    match v {
        None => f64::NAN,
        Some(Value::Null) => 0.0,
        Some(Value::Bool(b)) => f64::from(u8::from(*b)),
        Some(Value::Number(n)) => n.as_f64().unwrap_or(f64::NAN),
        Some(Value::String(s)) => js_string_to_number(s),
        Some(other @ Value::Array(_)) => js_string_to_number(&js_string(other)),
        Some(Value::Object(_)) => f64::NAN,
    }
}

/// Reads the token endpoint's answer (`exchangeCode` after the fetch), in the JS order of checks.
pub fn parse_token_response(status: u16, body: &[u8]) -> Result<TokenGrant, OAuthError> {
    if !(200..300).contains(&status) {
        return Err(OAuthError { message: format!("token exchange returned HTTP {status}"), status: Some(status) });
    }
    let data: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let field = |k: &str| data.as_object().and_then(|o| o.get(k));
    let scopes: Vec<&str> = match field("scope") {
        Some(Value::String(s)) => s.split(' ').collect(),
        _ => Vec::new(),
    };
    let access_token = match field("access_token") {
        Some(Value::String(t)) if !t.is_empty() && utf16_len(t) <= 512 => t.clone(),
        _ => return Err(OAuthError::plain("token exchange returned no access token")),
    };
    let token_type = field("token_type").map_or_else(|| "undefined".to_string(), js_string);
    if token_type.to_lowercase() != "bearer" {
        return Err(OAuthError::plain("token exchange returned an unexpected token type"));
    }
    if !OAUTH_SCOPES.iter().all(|s| scopes.contains(s)) {
        return Err(OAuthError::plain("token exchange is missing a required scope"));
    }
    let expires_in = js_number(field("expires_in"));
    Ok(TokenGrant {
        access_token,
        expires_in: if expires_in.is_finite() && expires_in > 0.0 { expires_in } else { SESSION_MAX_AGE_SECONDS as f64 },
    })
}

/// Exchanges an authorization code through any transport: `send` performs the POST and answers `(status, body)`, or
/// `Err(message)` when the request itself failed (network, timeout, a redirect).
pub async fn exchange_code_with<F, Fut>(
    creds: &OAuthCredentials,
    token_url: &str,
    code: &str,
    send: F,
) -> Result<TokenGrant, OAuthError>
where
    F: FnOnce(OAuthRequest) -> Fut,
    Fut: Future<Output = Result<(u16, Vec<u8>), String>>,
{
    let (status, body) = send(token_request(creds, token_url, code)).await.map_err(|m| OAuthError { message: m, status: None })?;
    parse_token_response(status, &body)
}

/// Revokes an access token through any transport. Best effort: true when Discord accepted the revocation.
pub async fn revoke_token_with<F, Fut>(creds: &OAuthCredentials, revoke_url: &str, access_token: &str, send: F) -> bool
where
    F: FnOnce(OAuthRequest) -> Fut,
    Fut: Future<Output = Result<(u16, Vec<u8>), String>>,
{
    matches!(send(revoke_request(creds, revoke_url, access_token)).await, Ok((status, _)) if (200..300).contains(&status))
}

/// The two outbound OAuth calls over reqwest: a 10 s timeout and no redirects followed (fetch's `redirect: 'error'`).
#[derive(Debug, Clone)]
pub struct OAuthClient {
    http: reqwest::Client,
    pub token_url: String,
    pub revoke_url: String,
}

impl Default for OAuthClient {
    fn default() -> Self {
        Self::new()
    }
}

impl OAuthClient {
    /// A client for Discord's real endpoints.
    pub fn new() -> Self {
        Self::with_endpoints(TOKEN_URL, TOKEN_REVOCATION_URL)
    }

    /// A client for other endpoints (tests, a proxy).
    pub fn with_endpoints(token_url: &str, revoke_url: &str) -> Self {
        let http = reqwest::Client::builder()
            // What Node's fetch sends, for wire parity with the JS token and revoke calls.
            .user_agent("node")
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_millis(OAUTH_TIMEOUT_MS))
            .build()
            .expect("the OAuth HTTP client (TLS backend) builds");
        Self { http, token_url: token_url.to_string(), revoke_url: revoke_url.to_string() }
    }

    /// Sends one form POST; a redirect answer is a failed request, as with fetch's `redirect: 'error'`.
    pub async fn send(&self, req: OAuthRequest) -> Result<(u16, Vec<u8>), String> {
        let res = self
            .http
            .post(&req.url)
            .header(reqwest::header::AUTHORIZATION, req.authorization)
            .header(reqwest::header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(reqwest::header::ACCEPT, "application/json")
            .body(req.body)
            .send()
            .await
            .map_err(|e| {
                // The messages of the JS fetch: AbortSignal.timeout's TimeoutError, else undici's TypeError.
                if e.is_timeout() { "The operation was aborted due to timeout" } else { "fetch failed" }.to_string()
            })?;
        let status = res.status().as_u16();
        if (300..400).contains(&status) {
            return Err("fetch failed".to_string());
        }
        if !(200..300).contains(&status) {
            // The body is not read (the JS cancels it).
            return Ok((status, Vec::new()));
        }
        let body = res.bytes().await.map(|b| b.to_vec()).unwrap_or_default();
        Ok((status, body))
    }

    /// `exchangeCode`: exchanges an authorization code for an access token.
    pub async fn exchange_code(&self, creds: &OAuthCredentials, code: &str) -> Result<TokenGrant, OAuthError> {
        exchange_code_with(creds, &self.token_url, code, |r| self.send(r)).await
    }

    /// `revokeToken`: revokes an access token. Best effort: never fails.
    pub async fn revoke_token(&self, creds: &OAuthCredentials, access_token: &str) -> bool {
        revoke_token_with(creds, &self.revoke_url, access_token, |r| self.send(r)).await
    }
}

// ── Access rules ──────────────────────────────────────────────────────────────

static PERMISSIONS_RE: LazyLock<JsRegex> = LazyLock::new(|| JsRegex::new(r"^\d{1,30}$", ""));

/// Whether a user may manage a guild from the dashboard (the bot's ADMIN level): the guild must be in the user's
/// OAuth guild list (`None` when it is not), and the user owns it or has Administrator or Manage Server there.
/// BOT_OWNER_ID gets no bypass. Takes the raw entry of `GET /users/@me/guilds`.
pub fn can_manage_guild(oauth_guild: Option<&Value>) -> bool {
    let Some(guild) = oauth_guild else { return false };
    let Some(obj) = guild.as_object() else { return false };
    if obj.get("owner") == Some(&Value::Bool(true)) {
        return true;
    }
    let Some(Value::String(raw)) = obj.get("permissions") else { return false };
    if !PERMISSIONS_RE.test(raw) {
        return false;
    }
    let Ok(perms) = raw.parse::<u128>() else { return false };
    perms & ADMINISTRATOR == ADMINISTRATOR || perms & MANAGE_GUILD == MANAGE_GUILD
}

/// Whether an Origin header is a well-formed http(s) origin (not `null`, not a path).
pub fn is_http_origin(value: &str) -> bool {
    match url::Url::parse(value) {
        Ok(u) => (u.scheme() == "http" || u.scheme() == "https") && u.origin().ascii_serialization() == value,
        Err(_) => false,
    }
}

/// The request headers `checkWriteRequest` reads, as Node reports them (`None` = not sent). Node keeps only the first
/// `Content-Type` and joins repeated other headers with `, `.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WriteHeaders {
    pub content_type: Option<String>,
    pub x_dashboard: Option<String>,
    pub origin: Option<String>,
    pub sec_fetch_site: Option<String>,
}

/// Why a write was refused: the HTTP status, the `error` code and the message of the JSON answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteRejection {
    pub status: u16,
    pub error: &'static str,
    pub message: String,
}

/// CSRF guard for state-changing requests: POST/PATCH/DELETE only, a JSON content type, `X-Dashboard: 1`, an Origin
/// (when sent) equal to the dashboard's origin, and Sec-Fetch-Site (when sent) same-origin or none. A different but
/// well-formed http(s) Origin gets a message naming the configured origin (never the request's).
pub fn check_write_request(method: &str, headers: &WriteHeaders, config_origin: &str) -> Result<(), WriteRejection> {
    let reject = |status, error, message: &str| Err(WriteRejection { status, error, message: message.to_string() });
    if method != "POST" && method != "PATCH" && method != "DELETE" {
        return reject(405, "method_not_allowed", "Method not allowed.");
    }
    let content_type = trim(headers.content_type.as_deref().unwrap_or("")).to_lowercase();
    if !content_type.starts_with("application/json") {
        return reject(415, "invalid_request", "Content-Type must be application/json.");
    }
    if headers.x_dashboard.as_deref() != Some("1") {
        return reject(403, "forbidden", "Missing the X-Dashboard header.");
    }
    if let Some(origin) = headers.origin.as_deref().filter(|o| *o != config_origin) {
        // A well-formed origin that merely differs is most likely the wrong address in DASHBOARD_URL.
        let message = if is_http_origin(origin) {
            format!(
                "This dashboard is configured for {config_origin}. Open it there, or set DASHBOARD_URL to the address you use."
            )
        } else {
            "Cross-origin requests are not allowed.".to_string()
        };
        return Err(WriteRejection { status: 403, error: "forbidden", message });
    }
    if headers.sec_fetch_site.as_deref().is_some_and(|site| site != "same-origin" && site != "none") {
        return reject(403, "forbidden", "Cross-site requests are not allowed.");
    }
    Ok(())
}
