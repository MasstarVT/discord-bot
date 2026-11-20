//! Port of `src/dashboard/server.js`: the dashboard's HTTP server. JSON API (settings, server resources, per-server
//! records), OAuth2 login, static UI files and security headers.
//!
//! [`create_dashboard_router`] returns an axum [`Router`] (not listening; [`crate::serve::serve`] runs it). Settings,
//! records and Discord access are injected through [`DashboardDeps`], so this module never opens Redis or Postgres
//! connections itself. Routing is one fallback handler that matches the raw request path the way the JS does, so the
//! order of the checks (method, CSRF, session, ids, rate limits, body, permissions) is the JS order.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::extract::{ConnectInfo, Request, State};
use axum::response::Response;
use bot_util::js::utf16_len;
use bot_util::jsre::JsRegex;
use dash_auth::auth::{
    CookieOptions, SESSION_COOKIE, SESSION_MAX_AGE_SECONDS, STATE_COOKIE, STATE_MAX_AGE_SECONDS, SessionKey, WriteHeaders,
    build_authorize_url, can_manage_guild, check_write_request, clear_cookie, create_oauth_state, derive_session_key, parse_cookies,
    seal_session, serialize_cookie, states_match, unseal_session,
};
use dash_auth::config::{Contact, ContactKind, normalize_ip};
use dash_auth::rate_limit::{LoginLimiter, RateDecision, RateLimiter, SEARCH_RATE_LIMIT, WRITE_RATE_LIMIT, client_address, login_source_key};
use dash_auth::site_parts::escape_html;
use dash_auth::DashboardConfig;
use dash_collections::discord::{DashboardDiscord, DiscordApiError, DiscordResources};
use dash_collections::store::{Actor, DashboardMember, DashboardUser, ListOpts, StoreError};
use dash_collections::{Collection, Collections, Mode, RecordsBridge, SettingsBridge, SettingsWriteCtx, SettingsWriteError};
use dash_schema::{ChannelInfo, GuildResources, KeepRefs, Refs, RoleInfo, RoleRef};
use futures::{FutureExt, StreamExt};
use http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use indexmap::{IndexMap, IndexSet};
use parking_lot::Mutex;
use serde_json::{Map, Value, json};

use crate::collate::name_compare;
use crate::discord_api::{OAuthGuild, ServerDiscord, guild_icon_url, user_avatar_url};
use crate::landing::{PageOptions, render_changelog, render_docs, render_landing, render_legal};
use crate::logger::{BotLogger, DashLogger};
use crate::static_files::{LoadError, PublicDir, StaticEntry, accepts_gzip, etag_matches, make_entry, static_file};
use crate::util::{clean_query, for_log, is_snowflake, js_int, parse_query, query_get};

/// Body limit of a settings PATCH (a full page of messages and embeds fits).
pub const BODY_LIMIT_BYTES: usize = 256 * 1024;
/// Body limit of a record write.
pub const RECORD_BODY_LIMIT_BYTES: usize = 128 * 1024;
pub const PROPAGATION_SECONDS: u64 = 30;
/// Channels tried for a bot-owner invite before giving up.
const OWNER_INVITE_ATTEMPTS: usize = 5;

/// The gzip copy of the catalog (level 9, as the static files), None when it would not be smaller.
fn catalog_gzip() -> Option<&'static [u8]> {
    static PACKED: LazyLock<Option<Vec<u8>>> =
        LazyLock::new(|| make_entry(dash_collections::catalog_json().as_bytes().to_vec(), "text/plain").gzip);
    PACKED.as_deref()
}
/// Where the dashboard app lives (the landing page has `/`).
const DASHBOARD_PATH: &str = "/dashboard";
const LOGIN_FAILED: &str = "/dashboard?error=login_failed";
const STATE_PATH: &str = "/auth";
/// A request whose body has not arrived within this time is answered 408 (Node's `server.requestTimeout`).
pub const REQUEST_TIMEOUT_MS: u64 = 15_000;
const CONTACT_MARKER: &str = "<!--contact-->";
const NO_CONTACT: &str = "the bot owner, through the server where you use the bot";
/// A records list's ?query= (searchable collections) and ?cursor= (the next page).
const LIST_QUERY_MAX: usize = 100;

/// The security headers of every answer (`SECURITY_HEADERS`), in the JS order.
pub const SECURITY_HEADERS: [(&str, &str); 6] = [
    (
        "Content-Security-Policy",
        "default-src 'self'; img-src 'self' https://cdn.discordapp.com data:; style-src 'self' https://fonts.googleapis.com; font-src https://fonts.gstatic.com; script-src 'self'; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'",
    ),
    ("X-Content-Type-Options", "nosniff"),
    ("Referrer-Policy", "same-origin"),
    ("X-Frame-Options", "DENY"),
    ("Cross-Origin-Opener-Policy", "same-origin"),
    ("Permissions-Policy", "camera=(), microphone=(), geolocation=()"),
];

static ACTION_ID_RE: LazyLock<JsRegex> = LazyLock::new(|| JsRegex::new(r"^[A-Za-z][A-Za-z0-9_-]{0,39}$", ""));
static LIST_CURSOR_RE: LazyLock<JsRegex> = LazyLock::new(|| JsRegex::new(r"^[A-Za-z0-9_:.-]{1,200}$", ""));
static RECORD_ID_RE: LazyLock<JsRegex> = LazyLock::new(|| JsRegex::new(r"^[A-Za-z0-9_-]{1,40}$", ""));
static CODE_RE: LazyLock<JsRegex> = LazyLock::new(|| JsRegex::new(r"^[\w.~-]+$", ""));
static GUILD_ROUTE_RE: LazyLock<JsRegex> =
    LazyLock::new(|| JsRegex::new(r"^\/api\/guilds\/([^/]{1,32})\/(settings|resources|members|invite)$", ""));
static RECORDS_ROUTE_RE: LazyLock<JsRegex> = LazyLock::new(|| {
    JsRegex::new(r"^\/api\/guilds\/([^/]{1,32})\/records\/([^/]{1,64})(?:\/([^/]{1,64})(?:\/actions\/([^/]{1,64}))?)?$", "")
});

/// A clock: ms since epoch (`deps.now`).
pub type Clock = Arc<dyn Fn() -> f64 + Send + Sync>;

/// Who added the bot to a server, as the bot recorded it (a BotJoin row: botJoinService.listBotJoins).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BotJoinRow {
    pub inviter_id: Option<String>,
    pub inviter_name: Option<String>,
    pub source: Option<String>,
    /// `joinedAt.toISOString()`.
    pub joined_at: String,
}

/// server.js `deps.botJoins` (src/services/botJoinService.js): the BotJoin rows of some servers.
#[async_trait]
pub trait BotJoins: Send + Sync {
    /// `listBotJoins(guildIds)`: guild id -> row, for the servers that have one.
    async fn list_bot_joins(&self, guild_ids: &[String]) -> anyhow::Result<HashMap<String, BotJoinRow>>;
}

/// server.js `deps`: `{ discord, settings, records?, botJoins?, collections?, now? }`.
#[derive(Clone)]
pub struct DashboardDeps {
    pub discord: Arc<dyn ServerDiscord>,
    pub settings: Arc<dyn SettingsBridge>,
    /// None: the records routes answer 503 unavailable.
    pub records: Option<Arc<dyn RecordsBridge>>,
    /// None: the bot owner's server list has no `addedBy`.
    pub bot_joins: Option<Arc<dyn BotJoins>>,
    /// The record registry (default: the built-in collections).
    pub collections: Option<Collections>,
    pub now: Option<Clock>,
}

/// `createDashboardServer({ config, deps, logger, publicDir })`.
#[derive(Clone)]
pub struct ServerOptions {
    pub config: DashboardConfig,
    pub deps: DashboardDeps,
    /// Default: the bot's logger.
    pub logger: Option<Arc<dyn DashLogger>>,
    /// The static UI files (default: embedded at build time).
    pub public_dir: PublicDir,
    /// CHANGELOG.md (default: embedded at build time). A file that cannot be read counts as empty.
    pub changelog_path: Option<PathBuf>,
}

impl ServerOptions {
    pub fn new(config: DashboardConfig, deps: DashboardDeps) -> Self {
        let _ = catalog_gzip(); // compressed once, at startup
        Self { config, deps, logger: None, public_dir: PublicDir::Embedded, changelog_path: None }
    }
}

/// The HTML a legal page shows for the configured contact, escaped (`contactHtml`).
pub fn contact_html(contact: Option<&Contact>) -> String {
    let Some(c) = contact else { return escape_html(NO_CONTACT) };
    let href = if c.kind == ContactKind::Email { format!("mailto:{}", c.value) } else { c.value.clone() };
    format!("<a href=\"{}\">{}</a>", escape_html(&href), escape_html(&c.value))
}

/// `formatColor(n)`: '#RRGGBB' for an integer 0..0xFFFFFF, else null.
fn format_color(v: Option<&Value>) -> Value {
    match js_int(v) {
        Some(n) if (0..=0xff_ffff).contains(&n) => Value::String(format!("#{n:06X}")),
        _ => Value::Null,
    }
}

/// The session a request carries (`isValidSession` passed).
#[derive(Debug, Clone)]
struct Session {
    uid: String,
    username: String,
    global_name: Option<String>,
    avatar: Option<String>,
    access_token: String,
}

fn valid_session(s: &Map<String, Value>) -> Option<Session> {
    let uid = s.get("uid")?.as_str().filter(|u| is_snowflake(u))?;
    let token = s.get("accessToken")?.as_str().filter(|t| !t.is_empty() && utf16_len(t) <= 512)?;
    let username = s.get("username")?.as_str()?;
    let nullable = |k: &str| -> Option<Option<String>> {
        match s.get(k)? {
            Value::Null => Some(None),
            Value::String(v) => Some(Some(v.clone())),
            _ => None,
        }
    };
    Some(Session {
        uid: uid.to_string(),
        username: username.to_string(),
        global_name: nullable("globalName")?,
        avatar: nullable("avatar")?,
        access_token: token.to_string(),
    })
}

/// The guild entry the routes use (`managedGuild`).
#[derive(Debug, Clone)]
struct ManagedGuild {
    id: String,
    name: String,
    icon: Option<String>,
    owner: bool,
    permissions: String,
}

/// One request in flight (`ctx`): the parsed target, the cookies in and out, the headers, the peer and the body.
struct Ctx {
    method: String,
    path: String,
    query: Vec<(String, String)>,
    cookie_in: IndexMap<String, String>,
    cookies: Vec<String>,
    headers: HeaderMap,
    peer: Option<String>,
    /// The request body (behind a mutex so the context is Sync across awaits).
    body: std::sync::Mutex<Option<Body>>,
}

impl Ctx {
    /// A header as Node reports it: repeated values joined with ', '.
    fn header(&self, name: &str) -> Option<String> {
        let values: Vec<String> = self.headers.get_all(name).iter().map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned()).collect();
        if values.is_empty() { None } else { Some(values.join(", ")) }
    }

    /// The first value only (Node keeps the first Content-Type).
    fn first_header(&self, name: &str) -> Option<String> {
        self.headers.get(name).map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned())
    }

    fn query(&self, name: &str) -> Option<&str> {
        query_get(&self.query, name)
    }
}

/// A route that stopped early with its answer. Anything unexpected (a panic of a dependency) becomes the 500 of
/// `handle`.
enum Stop {
    Answer(Box<Response>),
}

impl Stop {
    fn answer(res: Response) -> Self {
        Self::Answer(Box::new(res))
    }
}

type R<T> = Result<T, Stop>;

struct App {
    config: DashboardConfig,
    discord: Arc<dyn ServerDiscord>,
    settings: Arc<dyn SettingsBridge>,
    records: Option<Arc<dyn RecordsBridge>>,
    bot_joins: Option<Arc<dyn BotJoins>>,
    collections: Collections,
    now: Clock,
    logger: Arc<dyn DashLogger>,
    session_key: SessionKey,
    secure: bool,
    limit_writes: RateLimiter,
    limit_searches: RateLimiter,
    logins: LoginLimiter,
    trusted_proxies: HashSet<String>,
    public_dir: PublicDir,
    changelog_path: Option<PathBuf>,
    static_entries: Mutex<HashMap<String, Arc<StaticEntry>>>,
}

/// Builds the dashboard's router (`createDashboardServer`). Fails when the session secret cannot derive a key.
pub fn create_dashboard_router(opts: ServerOptions) -> Result<Router, String> {
    let session_key = derive_session_key(&opts.config.session_secret).map_err(str::to_string)?;
    let deps = opts.deps;
    let app = App {
        secure: opts.config.secure_cookies,
        trusted_proxies: opts.config.trusted_proxies.iter().map(|p| normalize_ip(p)).collect(),
        config: opts.config,
        discord: deps.discord,
        settings: deps.settings,
        records: deps.records,
        bot_joins: deps.bot_joins,
        collections: deps.collections.unwrap_or_else(|| Collections::builtin().clone()),
        now: deps.now.unwrap_or_else(|| Arc::new(bot_util::js::now_ms)),
        logger: opts.logger.unwrap_or_else(|| Arc::new(BotLogger)),
        session_key,
        limit_writes: RateLimiter::new(WRITE_RATE_LIMIT),
        limit_searches: RateLimiter::new(SEARCH_RATE_LIMIT),
        logins: LoginLimiter::new(),
        public_dir: opts.public_dir,
        changelog_path: opts.changelog_path,
        static_entries: Mutex::new(HashMap::new()),
    };
    Ok(Router::new().fallback(handle).with_state(Arc::new(app)))
}

fn status_of(code: u16) -> StatusCode {
    StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
}

async fn handle(State(app): State<Arc<App>>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let peer = parts.extensions.get::<ConnectInfo<SocketAddr>>().map(|c| c.0.ip().to_string());
    let target = parts.uri.path_and_query().map(|p| p.as_str().to_string()).unwrap_or_default();
    let mut ctx = Ctx {
        method: parts.method.as_str().to_string(),
        path: "/".to_string(),
        query: Vec::new(),
        cookie_in: IndexMap::new(),
        cookies: Vec::new(),
        headers: parts.headers,
        peer,
        body: std::sync::Mutex::new(Some(body)),
    };
    if parts.uri.scheme().is_some() || parts.uri.authority().is_some() || !target.starts_with('/') {
        return app.fail(&ctx, 400, "invalid_request", "Bad request target.", Map::new(), &[]);
    }
    match target.find('?') {
        Some(q) => {
            ctx.path = target[..q].to_string();
            ctx.query = parse_query(&target[q + 1..]);
        }
        None => ctx.path = target.clone(),
    }
    // Node joins repeated Cookie headers with '; ' (other headers with ', ').
    let cookies: Vec<String> = ctx.headers.get_all("cookie").iter().map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned()).collect();
    ctx.cookie_in = parse_cookies(if cookies.is_empty() { None } else { Some(cookies.join("; ")) }.as_deref());
    let method = ctx.method.clone();
    let path = ctx.path.clone();
    let outcome = AssertUnwindSafe(app.route(&mut ctx)).catch_unwind().await;
    let err = match outcome {
        Ok(Ok(res)) => return res,
        Ok(Err(Stop::Answer(res))) => return *res,
        Err(panic) => panic
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
            .unwrap_or_else(|| "the handler panicked".to_string()),
    };
    app.logger.error(&format!("Dashboard: {} {} failed", for_log(&method), for_log(&path)), Some(&err));
    ctx.cookies.clear();
    app.fail(&ctx, 500, "internal_error", "Something went wrong.", Map::new(), &[])
}

impl App {
    fn now(&self) -> f64 {
        (self.now)()
    }

    // ── Response helpers ────────────────────────────────────────────────────────

    fn send(&self, ctx: &Ctx, status: u16, body: Option<Vec<u8>>, headers: &[(&str, String)]) -> Response {
        let mut out: Vec<(String, String)> = SECURITY_HEADERS.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        if self.secure {
            out.push(("Strict-Transport-Security".into(), "max-age=31536000".into()));
        }
        for (k, v) in headers {
            match out.iter_mut().find(|(n, _)| n.eq_ignore_ascii_case(k)) {
                Some(slot) => slot.1 = v.clone(),
                None => out.push((k.to_string(), v.clone())),
            }
        }
        let bodiless = status == 204 || status == 304;
        if !bodiless {
            out.push(("Content-Length".into(), body.as_ref().map_or(0, Vec::len).to_string()));
        }
        let mut res = Response::new(if bodiless || ctx.method == "HEAD" { Body::empty() } else { Body::from(body.unwrap_or_default()) });
        *res.status_mut() = status_of(status);
        let map = res.headers_mut();
        for (k, v) in out {
            if let (Ok(name), Ok(value)) = (HeaderName::from_bytes(k.as_bytes()), HeaderValue::from_bytes(v.as_bytes())) {
                map.append(name, value);
            }
        }
        for c in &ctx.cookies {
            if let Ok(value) = HeaderValue::from_bytes(c.as_bytes()) {
                map.append(http::header::SET_COOKIE, value);
            }
        }
        res
    }

    fn json(&self, ctx: &Ctx, status: u16, payload: Value, headers: &[(&str, String)]) -> Response {
        let body = serde_json::to_string(&dash_schema::js::canon(payload)).unwrap_or_default().into_bytes();
        let mut all = vec![("Content-Type", "application/json; charset=utf-8".to_string()), ("Cache-Control", "no-store".to_string())];
        all.extend(headers.iter().cloned());
        self.send(ctx, status, Some(body), &all)
    }

    fn fail(&self, ctx: &Ctx, status: u16, error: &str, message: &str, extra: Map<String, Value>, headers: &[(&str, String)]) -> Response {
        let mut payload = Map::new();
        payload.insert("error".into(), json!(error));
        payload.insert("message".into(), json!(message));
        payload.extend(extra);
        self.json(ctx, status, Value::Object(payload), headers)
    }

    fn stop(&self, ctx: &Ctx, status: u16, error: &str, message: &str) -> Stop {
        Stop::answer(self.fail(ctx, status, error, message, Map::new(), &[]))
    }

    fn redirect(&self, ctx: &Ctx, location: &str) -> Response {
        self.send(ctx, 302, None, &[("Location", location.to_string()), ("Cache-Control", "no-store".to_string())])
    }

    fn allow(&self, ctx: &Ctx, methods: &[&str]) -> R<()> {
        if methods.contains(&ctx.method.as_str()) {
            return Ok(());
        }
        Err(Stop::answer(self.fail(ctx, 405, "method_not_allowed", "Method not allowed.", Map::new(), &[("Allow", methods.join(", "))])))
    }

    fn rate_limited(&self, ctx: &Ctx, message: String, retry_after: u64) -> Stop {
        let mut extra = Map::new();
        extra.insert("retryAfter".into(), json!(retry_after));
        Stop::answer(self.fail(ctx, 429, "rate_limited", &message, extra, &[("Retry-After", retry_after.to_string())]))
    }

    fn check_write(&self, ctx: &Ctx) -> R<()> {
        let headers = WriteHeaders {
            content_type: ctx.first_header("content-type"),
            x_dashboard: ctx.header("x-dashboard"),
            origin: ctx.header("origin"),
            sec_fetch_site: ctx.header("sec-fetch-site"),
        };
        check_write_request(&ctx.method, &headers, &self.config.origin).map_err(|r| self.stop(ctx, r.status, r.error, &r.message))
    }

    // ── Session ─────────────────────────────────────────────────────────────────

    fn read_session(&self, ctx: &Ctx) -> (Option<Session>, bool) {
        let Some(raw) = ctx.cookie_in.get(SESSION_COOKIE) else { return (None, false) };
        match unseal_session(Some(raw), &self.session_key, self.now()).as_ref().and_then(valid_session) {
            Some(s) => (Some(s), false),
            None => (None, true),
        }
    }

    fn clear_session(&self, ctx: &mut Ctx) {
        ctx.cookies.push(clear_cookie(SESSION_COOKIE, "/", self.secure));
    }

    fn require_session(&self, ctx: &mut Ctx) -> R<Session> {
        let (session, stale) = self.read_session(ctx);
        if let Some(s) = session {
            return Ok(s);
        }
        if stale {
            self.clear_session(ctx);
        }
        Err(self.stop(ctx, 401, "unauthenticated", "Please log in with Discord."))
    }

    fn guild_id_ok(&self, ctx: &Ctx, guild_id: &str) -> R<()> {
        if is_snowflake(guild_id) { Ok(()) } else { Err(self.stop(ctx, 400, "invalid_request", "That is not a valid server id.")) }
    }

    /// Answers a failed Discord call (`discordFailure`). `user_token`: the call used the session's token (a 401 ends
    /// the session).
    fn discord_failure(&self, ctx: &mut Ctx, err: &DiscordApiError, user_token: bool) -> Stop {
        if err.code == "unauthenticated" && user_token {
            self.clear_session(ctx);
            return self.stop(ctx, 401, "unauthenticated", "Your Discord login has expired. Please log in again.");
        }
        if err.code == "rate_limited" {
            let retry_after = err.retry_after.unwrap_or(1);
            return self.rate_limited(ctx, format!("Discord is rate limiting requests. Try again in {retry_after} s."), retry_after);
        }
        self.logger.warn(&format!("Dashboard: Discord request failed: {}", err.message));
        self.stop(ctx, 502, "discord_unavailable", "Discord could not be reached. Try again in a moment.")
    }

    /// The user's OAuth guild list; on a Discord failure answers. `allow_stale`: display only.
    async fn user_guilds(&self, ctx: &mut Ctx, session: &Session, allow_stale: bool) -> R<Arc<Vec<OAuthGuild>>> {
        match self.discord.get_user_guilds(&session.uid, &session.access_token, allow_stale).await {
            Ok(g) => Ok(g),
            Err(e) => Err(self.discord_failure(ctx, &e, true)),
        }
    }

    /// BOT_OWNER_ID may manage every server the bot is in.
    fn is_bot_owner(&self, session: &Session) -> bool {
        self.config.bot_owner_id.as_deref().is_some_and(|o| !o.is_empty() && o == session.uid)
    }

    /// The guild entry the routes use when the user may manage the guild and the bot is in it (`managedGuild`).
    ///
    /// The bot's presence is always checked against a list at most BOT_GUILDS_TTL_MS old (refreshed ahead of expiry), for
    /// every method: after the bot is removed, the next read past the TTL answers bot_not_in_guild. Only the picker list
    /// and name/icon lookups may use the stale-while-revalidate list.
    async fn managed_guild(&self, ctx: &mut Ctx, session: &Session, guild_id: &str) -> R<ManagedGuild> {
        let guilds = self.user_guilds(ctx, session, false).await?;
        let guild = guilds.iter().find(|g| g.id == guild_id).cloned();
        let bot_owner = self.is_bot_owner(session);
        // Checked before the bot's presence, so other users cannot probe which servers the bot is in.
        if !bot_owner && !can_manage_guild(guild.as_ref().map(OAuthGuild::to_json).as_ref()) {
            return Err(self.stop(ctx, 403, "forbidden", "You need the Manage Server or Administrator permission in this server."));
        }
        let present = match self.discord.is_bot_in_guild(guild_id, true).await {
            Ok(p) => p,
            Err(e) => return Err(self.discord_failure(ctx, &e, false)),
        };
        if !present {
            let mut extra = Map::new();
            extra.insert("inviteUrl".into(), json!(self.discord.invite_url(Some(guild_id))));
            return Err(Stop::answer(self.fail(ctx, 404, "bot_not_in_guild", "The bot is not in this server yet.", extra, &[])));
        }
        if !bot_owner {
            let g = guild.unwrap_or_else(|| OAuthGuild {
                id: guild_id.to_string(),
                name: String::new(),
                icon: None,
                owner: false,
                permissions: "0".into(),
            });
            return Ok(ManagedGuild { id: g.id, name: g.name, icon: g.icon, owner: g.owner, permissions: g.permissions });
        }
        let (name, icon) = match &guild {
            Some(g) => (Some(g.name.clone()), g.icon.clone()),
            None => match self.discord.get_bot_guilds_swr().await {
                Ok(list) => match list.iter().find(|g| g.id == guild_id) {
                    Some(b) => (Some(b.name.clone()), b.icon.clone()),
                    None => (None, None),
                },
                Err(_) => (None, None),
            },
        };
        Ok(ManagedGuild {
            id: guild_id.to_string(),
            name: name.unwrap_or_else(|| "Unknown server".to_string()),
            icon,
            owner: true,
            permissions: guild.map_or_else(|| "0".to_string(), |g| g.permissions),
        })
    }

    // ── Routes ──────────────────────────────────────────────────────────────────

    async fn route(&self, ctx: &mut Ctx) -> R<Response> {
        let path = ctx.path.clone();
        match path.as_str() {
            "/healthz" => {
                self.allow(ctx, &["GET", "HEAD"])?;
                return Ok(self.json(ctx, 200, json!({ "ok": true }), &[]));
            }
            "/auth/login" => return self.login(ctx),
            "/auth/callback" => return self.callback(ctx).await,
            "/auth/logout" => return self.logout(ctx),
            "/api/me" => return self.me(ctx),
            "/api/catalog" => return self.catalog(ctx),
            "/api/guilds" => return self.guild_list(ctx).await,
            _ => {}
        }
        if let Some((text, m)) = GUILD_ROUTE_RE.exec(&path) {
            let guild_id = m.group(&text, 1).unwrap_or_default();
            return match m.group(&text, 2).as_deref() {
                Some("settings") => self.guild_settings(ctx, &guild_id).await,
                Some("resources") => self.guild_resources(ctx, &guild_id).await,
                Some("members") => self.guild_members(ctx, &guild_id).await,
                _ => self.owner_invite(ctx, &guild_id).await,
            };
        }
        if let Some((text, m)) = RECORDS_ROUTE_RE.exec(&path) {
            let guild_id = m.group(&text, 1).unwrap_or_default();
            let key = m.group(&text, 2).unwrap_or_default();
            return self.guild_records(ctx, &guild_id, &key, m.group(&text, 3), m.group(&text, 4)).await;
        }
        if let Some((file, content_type)) = static_file(&path) {
            return self.static_file(ctx, file, content_type).await;
        }
        let message = if path.starts_with("/api/") { "Unknown API route." } else { "Not found." };
        Ok(self.fail(ctx, 404, "not_found", message, Map::new(), &[]))
    }

    fn login(&self, ctx: &mut Ctx) -> R<Response> {
        self.allow(ctx, &["GET"])?;
        let state = create_oauth_state();
        let opts = CookieOptions { max_age: Some(STATE_MAX_AGE_SECONDS as f64), path: STATE_PATH.into(), secure: self.secure };
        ctx.cookies.push(serialize_cookie(STATE_COOKIE, &state, &opts));
        Ok(self.redirect(ctx, &build_authorize_url(&self.config.client_id, &self.config.redirect_uri, &state)))
    }

    async fn callback(&self, ctx: &mut Ctx) -> R<Response> {
        self.allow(ctx, &["GET"])?;
        let expected = ctx.cookie_in.get(STATE_COOKIE).cloned();
        ctx.cookies.push(clear_cookie(STATE_COOKIE, STATE_PATH, self.secure));
        let failed = |ctx: &Ctx, reason: &str| {
            self.logger.warn(&format!("Dashboard login failed: {reason}"));
            Ok(self.redirect(ctx, LOGIN_FAILED))
        };
        if let Some(error) = ctx.query("error") {
            let reason = format!("Discord answered \"{}\"", for_log(error));
            return failed(ctx, &reason);
        }
        if !states_match(expected.as_deref(), ctx.query("state")) {
            return failed(ctx, "the state did not match (expired or forged login)");
        }
        let code = ctx.query("code").unwrap_or_default().to_string();
        if code.is_empty() || utf16_len(&code) > 512 || !CODE_RE.test(&code) {
            return failed(ctx, "missing or malformed code");
        }

        // From here on the request costs a POST to Discord, so it is counted (see dash_auth::rate_limit::LoginLimiter).
        let address = client_address(ctx.peer.as_deref(), ctx.header("x-forwarded-for").as_deref(), &self.trusted_proxies);
        let source = login_source_key(&address);
        let check = self.logins.check(&source, self.now());
        if let RateDecision::Limited { retry_after } = check.decision {
            let which = if check.overall { "overall limit".to_string() } else { for_log(&address) };
            self.logger.warn(&format!("Dashboard login rate limited: {which}"));
            return Err(self.rate_limited(ctx, format!("Too many login attempts. Try again in {retry_after} s."), retry_after));
        }

        let token = match self.discord.exchange_code(&code).await {
            Ok(t) => t,
            Err(e) => return failed(ctx, &for_log(&e.to_string())),
        };
        let user = match self.discord.get_current_user(&token.access_token).await {
            Ok(u) => u,
            Err(e) => return failed(ctx, &for_log(&e.message)),
        };
        self.logins.resolved(&source);
        // The first page after login needs the user's server list: load it now, off the redirect's path.
        self.discord.warm_user_guilds(&user.id, &token.access_token);
        let issued = self.now();
        let exp = issued + token.expires_in.min(SESSION_MAX_AGE_SECONDS as f64) * 1000.0;
        let payload = json!({
            "uid": user.id, "username": user.username, "globalName": user.global_name, "avatar": user.avatar,
            "accessToken": token.access_token, "exp": dash_schema::js::js_num(exp),
        });
        let sealed = seal_session(&payload, &self.session_key);
        let opts = CookieOptions { max_age: Some((exp - issued) / 1000.0), path: "/".into(), secure: self.secure };
        ctx.cookies.push(serialize_cookie(SESSION_COOKIE, &sealed, &opts));
        self.logger.info(&format!("Dashboard login: {} ({})", for_log(&user.username), user.id));
        Ok(self.redirect(ctx, DASHBOARD_PATH))
    }

    fn logout(&self, ctx: &mut Ctx) -> R<Response> {
        self.allow(ctx, &["POST"])?;
        self.check_write(ctx)?;
        let (session, _) = self.read_session(ctx);
        self.clear_session(ctx);
        if let Some(session) = session {
            self.discord.forget_user(&session.uid);
            let discord = self.discord.clone();
            let token = session.access_token.clone();
            tokio::spawn(async move {
                let _ = discord.revoke_token(&token).await;
            });
            self.logger.info(&format!("Dashboard logout: {}", session.uid));
        }
        Ok(self.send(ctx, 204, None, &[("Cache-Control", "no-store".to_string())]))
    }

    fn me(&self, ctx: &mut Ctx) -> R<Response> {
        self.allow(ctx, &["GET"])?;
        let s = self.require_session(ctx)?;
        Ok(self.json(
            ctx,
            200,
            json!({ "user": {
                "id": s.uid, "username": s.username, "globalName": s.global_name,
                "avatarUrl": user_avatar_url(Some(&s.uid), s.avatar.as_deref()),
            } }),
            &[],
        ))
    }

    fn catalog(&self, ctx: &mut Ctx) -> R<Response> {
        self.allow(ctx, &["GET"])?;
        self.require_session(ctx)?;
        // The same public content for every user: one gzip copy, made once. No other API JSON is compressed (BREACH).
        let plain = dash_collections::catalog_json().as_bytes();
        let packed = catalog_gzip().filter(|_| accepts_gzip(ctx.header("accept-encoding").as_deref()));
        let zipped = packed.is_some();
        let mut headers = vec![
            ("Content-Type", "application/json; charset=utf-8".to_string()),
            ("Cache-Control", "no-store".to_string()),
            ("Vary", "Accept-Encoding".to_string()),
        ];
        if zipped {
            headers.push(("Content-Encoding", "gzip".to_string()));
        }
        let body = packed.map_or_else(|| plain.to_vec(), <[u8]>::to_vec);
        Ok(self.send(ctx, 200, Some(body), &headers))
    }

    async fn guild_list(&self, ctx: &mut Ctx) -> R<Response> {
        self.allow(ctx, &["GET"])?;
        let session = self.require_session(ctx)?;
        // The bot's list loads while the user's does (an error of either is handled below, as before).
        self.discord.prefetch_bot_guilds();
        let guilds = self.user_guilds(ctx, &session, true).await?;
        let manageable: Vec<&OAuthGuild> = guilds.iter().filter(|g| can_manage_guild(Some(&g.to_json()))).collect();
        let checks = futures::future::try_join_all(manageable.iter().map(|g| self.discord.is_bot_in_guild_swr(&g.id, false))).await;
        let present = match checks {
            Ok(p) => p,
            Err(e) => return Err(self.discord_failure(ctx, &e, false)),
        };
        let mut list: Vec<Map<String, Value>> = manageable
            .iter()
            .zip(present.iter())
            .map(|(g, &p)| {
                let mut m = Map::new();
                m.insert("id".into(), json!(g.id));
                m.insert("name".into(), json!(g.name));
                m.insert("iconUrl".into(), json!(guild_icon_url(Some(&g.id), g.icon.as_deref())));
                m.insert("botPresent".into(), json!(p));
                m.insert("inviteUrl".into(), if p { Value::Null } else { json!(self.discord.invite_url(Some(&g.id))) });
                m
            })
            .collect();
        let s = |m: &Map<String, Value>, k: &str| m.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
        list.sort_by(|a, b| {
            let pa = a.get("botPresent") == Some(&Value::Bool(true));
            let pb = b.get("botPresent") == Some(&Value::Bool(true));
            pb.cmp(&pa).then_with(|| name_compare(&s(a, "name"), &s(b, "name"))).then_with(|| s(a, "id").cmp(&s(b, "id")))
        });
        if !self.is_bot_owner(&session) {
            return Ok(self.json(ctx, 200, json!({ "guilds": list }), &[]));
        }
        let member_of: HashSet<&str> = guilds.iter().map(|g| g.id.as_str()).collect();
        let skip: HashSet<String> = list.iter().map(|g| s(g, "id")).collect();
        let mut owner_guilds = self.owner_guild_list(&skip, &member_of).await;
        self.add_inviters(&mut list, &mut owner_guilds).await;
        Ok(self.json(ctx, 200, json!({ "guilds": list, "ownerGuilds": owner_guilds }), &[]))
    }

    /// For the bot owner: sets `addedBy` on each server the bot is in, from the BotJoin rows (`addInviters`).
    async fn add_inviters(&self, list: &mut [Map<String, Value>], owner: &mut [Map<String, Value>]) {
        let Some(bot_joins) = &self.bot_joins else { return };
        let present = |m: &Map<String, Value>| m.get("botPresent") == Some(&Value::Bool(true));
        let ids: Vec<String> =
            list.iter().chain(owner.iter()).filter(|g| present(g)).filter_map(|g| g.get("id").and_then(Value::as_str).map(str::to_string)).collect();
        let rows = match bot_joins.list_bot_joins(&ids).await {
            Ok(r) => r,
            Err(e) => {
                self.logger.warn(&format!("Dashboard: could not read who added the bot: {e}"));
                return;
            }
        };
        for g in list.iter_mut().chain(owner.iter_mut()) {
            if !present(g) {
                continue;
            }
            let id = g.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
            let added = match rows.get(&id) {
                Some(r) => json!({ "userId": r.inviter_id, "name": r.inviter_name, "source": r.source, "joinedAt": r.joined_at }),
                None => Value::Null,
            };
            g.insert("addedBy".into(), added);
        }
    }

    /// For the bot owner: every server the bot is in that is not in their own list (`ownerGuildList`).
    async fn owner_guild_list(&self, skip: &HashSet<String>, member_of: &HashSet<&str>) -> Vec<Map<String, Value>> {
        let all = match self.discord.get_bot_guilds_swr().await {
            Ok(a) => a,
            Err(e) => {
                self.logger.warn(&format!("Dashboard: could not list the bot's servers for the bot owner: {}", e.message));
                return Vec::new();
            }
        };
        let mut out: Vec<(String, String, Map<String, Value>)> = all
            .iter()
            .filter(|g| !skip.contains(&g.id))
            .map(|g| {
                let mut m = Map::new();
                m.insert("id".into(), json!(g.id));
                m.insert("name".into(), json!(g.name));
                m.insert("iconUrl".into(), json!(guild_icon_url(Some(&g.id), g.icon.as_deref())));
                m.insert("botPresent".into(), json!(true));
                m.insert("inviteUrl".into(), Value::Null);
                m.insert("ownerAccess".into(), json!(true));
                m.insert("member".into(), json!(member_of.contains(g.id.as_str())));
                (g.name.clone(), g.id.clone(), m)
            })
            .collect();
        out.sort_by(|a, b| name_compare(&a.0, &b.0).then_with(|| a.1.cmp(&b.1)));
        out.into_iter().map(|(_, _, m)| m).collect()
    }

    /// POST /api/guilds/:id/invite: a single-use, 30-minute invite for the bot owner (`ownerInvite`).
    async fn owner_invite(&self, ctx: &mut Ctx, guild_id: &str) -> R<Response> {
        self.allow(ctx, &["POST"])?;
        self.check_write(ctx)?;
        let session = self.require_session(ctx)?;
        self.guild_id_ok(ctx, guild_id)?;
        if !self.is_bot_owner(&session) {
            return Err(self.stop(ctx, 403, "forbidden", "Only the bot owner can create an invite here."));
        }
        self.within_write_limit(ctx, &session)?;
        self.managed_guild(ctx, &session, guild_id).await?;
        let res = match self.discord.get_guild_resources(guild_id, false).await {
            Ok(r) => r,
            Err(e) => return Err(self.discord_failure(ctx, &e, false)),
        };
        if !res.bot.create_invite {
            return Err(self.stop(ctx, 422, "action_failed", "The bot needs the Create Invite permission in this server to invite you."));
        }
        let by_kind = |kinds: &[&str]| res.channels.iter().filter(|c| kinds.contains(&c.kind.as_str())).cloned().collect::<Vec<_>>();
        let mut candidates = by_kind(&["text", "announcement"]);
        candidates.extend(by_kind(&["voice", "stage"]));
        candidates.truncate(OWNER_INVITE_ATTEMPTS);
        for channel in candidates {
            match self.discord.create_invite(&channel.id, "Requested by the bot owner from the dashboard").await {
                Ok(invite) => {
                    self.logger.info(&format!("Dashboard: the bot owner created an invite for guild {guild_id} (channel {})", channel.id));
                    return Ok(self.json(
                        ctx,
                        200,
                        json!({ "url": invite.url, "expiresAt": invite.expires_at, "channel": { "id": channel.id, "name": channel.name } }),
                        &[],
                    ));
                }
                // 50013 Missing Permissions / 50001 Missing Access: a channel override; try the next one.
                Err(e) if matches!(e.discord_code, Some(50013 | 50001)) => continue,
                Err(e) => return Err(self.discord_failure(ctx, &e, false)),
            }
        }
        Err(self.stop(
            ctx,
            422,
            "action_failed",
            "The bot could not create an invite in the channels it tried. Let it Create Invite in a text channel.",
        ))
    }

    /// Answers 429 when the user is over the write rate limit.
    fn within_write_limit(&self, ctx: &Ctx, session: &Session) -> R<()> {
        match self.limit_writes.hit(&session.uid, self.now()) {
            RateDecision::Ok => Ok(()),
            RateDecision::Limited { retry_after } => {
                Err(self.rate_limited(ctx, format!("Too many changes at once. Try again in {retry_after} s."), retry_after))
            }
        }
    }

    fn within_search_limit(&self, ctx: &Ctx, session: &Session) -> R<()> {
        match self.limit_searches.hit(&session.uid, self.now()) {
            RateDecision::Ok => Ok(()),
            RateDecision::Limited { retry_after } => {
                Err(self.rate_limited(ctx, format!("Too many searches at once. Try again in {retry_after} s."), retry_after))
            }
        }
    }

    /// Reads a request body up to `limit` bytes (`readBody`): Err(413) when it is larger, Err(400) when it breaks off.
    async fn read_body(&self, ctx: &mut Ctx, limit: usize) -> Result<Vec<u8>, u16> {
        if let Some(declared) = ctx.header("content-length") {
            let n = dash_schema::js::to_number(Some(&Value::String(declared)));
            if n.is_finite() && n > limit as f64 {
                return Err(413);
            }
        }
        let taken = ctx.body.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take();
        let Some(body) = taken else { return Ok(Vec::new()) };
        let read = async move {
            let mut stream = body.into_data_stream();
            let mut out: Vec<u8> = Vec::new();
            while let Some(chunk) = stream.next().await {
                match chunk {
                    Ok(c) => {
                        if out.len() + c.len() > limit {
                            return Err(413);
                        }
                        out.extend_from_slice(&c);
                    }
                    Err(_) => return Err(400),
                }
            }
            Ok(out)
        };
        match tokio::time::timeout(Duration::from_millis(REQUEST_TIMEOUT_MS), read).await {
            Ok(r) => r,
            Err(_) => Err(408),
        }
    }

    /// Reads and parses a JSON body (`readJson`); on failure answers.
    async fn read_json(&self, ctx: &mut Ctx, limit: usize, optional: bool) -> R<Value> {
        let bytes = match self.read_body(ctx, limit).await {
            Ok(b) => b,
            Err(413) => {
                return Err(Stop::answer(self.fail(
                    ctx,
                    413,
                    "invalid_request",
                    "The request body is too large.",
                    Map::new(),
                    &[("Connection", "close".into())],
                )));
            }
            Err(408) => {
                let mut res = self.send(ctx, 408, None, &[("Connection", "close".into())]);
                res.headers_mut().remove(http::header::SET_COOKIE);
                return Err(Stop::answer(res));
            }
            Err(_) => return Err(self.stop(ctx, 400, "invalid_request", "The request body could not be read.")),
        };
        let text = String::from_utf8_lossy(&bytes);
        if optional && bot_util::js::trim(&text).is_empty() {
            return Ok(Value::Object(Map::new()));
        }
        serde_json::from_str(&text).map_err(|_| self.stop(ctx, 400, "invalid_request", "The request body is not valid JSON."))
    }

    /// Checks the channel and role ids of a write against the server (`verifyRefs`).
    #[allow(clippy::too_many_arguments)]
    async fn verify_refs(
        &self,
        ctx: &mut Ctx,
        session: &Session,
        guild_id: &str,
        refs: &Refs,
        keep: &KeepRefs,
        keep_keys: &IndexSet<String>,
        key_of: &(dyn Fn(&RoleRef) -> String + Sync),
    ) -> R<()> {
        if refs.channels.is_empty() && refs.roles.is_empty() {
            return Ok(());
        }
        let mut res = match self.discord.get_guild_resources(guild_id, false).await {
            Ok(r) => r,
            Err(e) => return Err(self.discord_failure(ctx, &e, false)),
        };
        // An id the cached lists do not know may be a channel or role created in the last few seconds.
        if !knows_all(refs, &res, keep) {
            res = match self.discord.get_guild_resources(guild_id, true).await {
                Ok(r) => r,
                Err(e) => return Err(self.discord_failure(ctx, &e, false)),
            };
        }
        let map = ref_resources(&res);
        if let Err(e) = dash_schema::check_refs(refs, &map, keep) {
            return Err(self.path_problem(ctx, &e.path, &e.message));
        }
        let given: Vec<&RoleRef> =
            refs.roles.iter().filter(|r| r.assignable && !keep_keys.contains(&key_of(r)) && res.role(&r.id).is_some()).collect();
        if given.is_empty() {
            return Ok(());
        }
        if !res.bot.manage_roles {
            return Err(self.path_problem(ctx, &given[0].path, "The bot needs the Manage Roles permission to give roles."));
        }
        for r in &given {
            // Level rewards and achievement role removals may use such roles, as /levelconfig and /achievement-config allow.
            if res.role(&r.id).and_then(|x| x.dangerous) == Some(true) && !r.allow_dangerous {
                return Err(self.path_problem(
                    ctx,
                    &r.path,
                    "That role has moderator or admin permissions (such as Administrator, Manage Server, Ban or Kick Members). The bot does not hand those out; pick a role without them.",
                ));
            }
        }
        if res.guild.owner_id.as_deref() == Some(session.uid.as_str()) || self.is_bot_owner(session) {
            return Ok(());
        }
        let member = match self.discord.get_member(guild_id, &session.uid).await {
            Ok(m) => m,
            Err(e) => return Err(self.discord_failure(ctx, &e, false)),
        };
        let top = top_position(&res, member.as_ref().map(|m| m.roles.as_slice()).unwrap_or(&[]));
        if let Some(above) = given.iter().find(|r| res.role(&r.id).map_or(0, |x| x.position) >= top) {
            return Err(self.path_problem(ctx, &above.path, "That role is not below your highest role, so you cannot have the bot give it."));
        }
        Ok(())
    }

    fn path_problem(&self, ctx: &Ctx, path: &str, message: &str) -> Stop {
        let mut extra = Map::new();
        extra.insert("path".into(), json!(path));
        Stop::answer(self.fail(ctx, 400, "invalid_request", message, extra, &[]))
    }

    /// For a collection that opts in (`checkStoredRoles`): every assignable role of the saved record must be below the
    /// user's highest role (`verifyStoredRoles`).
    async fn verify_stored_roles(
        &self,
        ctx: &mut Ctx,
        session: &Session,
        guild_id: &str,
        collection: &Collection,
        saved: &Value,
        changed: &Map<String, Value>,
    ) -> R<()> {
        let held = dash_schema::collect_assignable_roles(&collection.fields, Some(saved));
        if held.is_empty() {
            return Ok(());
        }
        let res = match self.discord.get_guild_resources(guild_id, false).await {
            Ok(r) => r,
            Err(e) => return Err(self.discord_failure(ctx, &e, false)),
        };
        if res.guild.owner_id.as_deref() == Some(session.uid.as_str()) || self.is_bot_owner(session) {
            return Ok(());
        }
        let member = match self.discord.get_member(guild_id, &session.uid).await {
            Ok(m) => m,
            Err(e) => return Err(self.discord_failure(ctx, &e, false)),
        };
        let top = top_position(&res, member.as_ref().map(|m| m.roles.as_slice()).unwrap_or(&[]));
        let Some(above) = held.iter().find(|r| res.role(&r.id).map_or(-1, |x| x.position) >= top) else { return Ok(()) };
        let field = above.path.split('.').next().unwrap_or_default();
        let mut extra = Map::new();
        if changed.contains_key(field) {
            extra.insert("path".into(), json!(above.path));
        }
        let message = format!("This {} gives or takes a role that is not below your highest role, so you cannot change it.", collection.singular());
        Err(Stop::answer(self.fail(ctx, 400, "invalid_request", &message, extra, &[])))
    }

    async fn load_row(&self, ctx: &Ctx, guild_id: &str) -> R<Value> {
        match self.settings.read_guild_settings(guild_id).await {
            Ok(row) => Ok(row),
            Err(e) => {
                self.logger.warn(&format!("Dashboard: could not load settings for guild {guild_id}: {e}"));
                Err(self.stop(ctx, 503, "settings_unavailable", "The settings could not be loaded. Try again in a moment."))
            }
        }
    }

    async fn guild_settings(&self, ctx: &mut Ctx, guild_id: &str) -> R<Response> {
        self.allow(ctx, &["GET", "PATCH"])?;
        if ctx.method == "PATCH" {
            self.check_write(ctx)?;
        }
        let session = self.require_session(ctx)?;
        self.guild_id_ok(ctx, guild_id)?;

        if ctx.method == "GET" {
            let guild = self.managed_guild(ctx, &session, guild_id).await?;
            let row = self.load_row(ctx, guild_id).await?;
            let str_or = |k: &str, d: &str| row.get(k).and_then(Value::as_str).unwrap_or(d).to_string();
            let payload = json!({
                "guild": { "id": guild.id, "name": guild.name, "iconUrl": guild_icon_url(Some(&guild.id), guild.icon.as_deref()) },
                "values": dash_schema::read_settings_values(Some(&row)),
                "general": {
                    "timezone": str_or("timezone", "UTC"),
                    "customPrefix": str_or("customPrefix", "!"),
                    "embedColor": format_color(row.get("embedColor")),
                },
                "propagationSeconds": PROPAGATION_SECONDS,
            });
            return Ok(self.json(ctx, 200, payload, &[]));
        }

        // PATCH
        self.within_write_limit(ctx, &session)?;
        let parsed = self.read_json(ctx, BODY_LIMIT_BYTES, false).await?;
        let valid = match dash_schema::validate_settings_changes(&parsed) {
            Ok(v) => v,
            Err(e) => {
                let mut extra = Map::new();
                if let Some(p) = e.path.filter(|p| !p.is_empty()) {
                    extra.insert("path".into(), json!(p));
                }
                return Err(Stop::answer(self.fail(ctx, 400, "invalid_request", &e.message, extra, &[])));
            }
        };
        self.managed_guild(ctx, &session, guild_id).await?;
        if !valid.refs.channels.is_empty() || !valid.refs.roles.is_empty() {
            let row = self.load_row(ctx, guild_id).await?;
            let keep = dash_schema::collect_settings_refs(Some(&row)).to_keep();
            let keys = dash_schema::collect_settings_ref_keys(Some(&row));
            let key_of = |r: &RoleRef| dash_schema::ref_key(&r.path, &r.id);
            self.verify_refs(ctx, &session, guild_id, &valid.refs, &keep, &keys, &key_of).await?;
        }
        let names: Vec<String> = dash_schema::js::js_keys(&valid.changes).into_iter().cloned().collect();
        let write_ctx = SettingsWriteCtx { user_id: session.uid.clone(), discord: Some(self.discord.clone() as Arc<dyn DashboardDiscord>) };
        let result = match self.settings.write_settings(guild_id, valid.changes, write_ctx).await {
            Ok(r) => r,
            Err(SettingsWriteError::Field(e)) => return Err(self.path_problem(ctx, &e.path, &e.message)),
            Err(SettingsWriteError::Internal(e)) => {
                self.logger.error(&format!("Dashboard: saving settings for guild {guild_id} failed"), Some(&e.to_string()));
                return Err(self.stop(ctx, 500, "save_failed", "The change could not be saved. Try again in a moment."));
            }
        };
        // Field names only: values can be long or private.
        self.logger.info(&format!("Dashboard: user {} changed guild {guild_id}: {}", session.uid, names.join(", ")));
        let mut payload = Map::new();
        payload.insert("values".into(), Value::Object(dash_schema::read_settings_values(Some(&result.row))));
        if !result.notices.is_empty() {
            payload.insert("notices".into(), json!(result.notices));
        }
        Ok(self.json(ctx, 200, Value::Object(payload), &[]))
    }

    async fn guild_resources(&self, ctx: &mut Ctx, guild_id: &str) -> R<Response> {
        self.allow(ctx, &["GET"])?;
        let session = self.require_session(ctx)?;
        self.guild_id_ok(ctx, guild_id)?;
        let fresh = ctx.query("fresh") == Some("1");
        // A forced reload costs Discord requests, so it counts towards the per-user limit.
        if fresh {
            self.within_write_limit(ctx, &session)?;
        }
        self.managed_guild(ctx, &session, guild_id).await?;
        // A display list: it may be up to 10 minutes old while it reloads in the background; `fresh` forces a live load.
        // Every check of ids or roles (verify_refs, the stores) uses the plain read, so it stays within 30 s.
        let res = match if fresh { self.discord.get_guild_resources(guild_id, true).await } else { self.discord.get_guild_resources_display(guild_id).await } {
            Ok(r) => r,
            Err(e) => return Err(self.discord_failure(ctx, &e, false)),
        };
        Ok(self.json(ctx, 200, public_resources(&res), &[]))
    }

    /// GET /api/guilds/:id/members?query=: member search for the user pickers.
    async fn guild_members(&self, ctx: &mut Ctx, guild_id: &str) -> R<Response> {
        self.allow(ctx, &["GET"])?;
        let session = self.require_session(ctx)?;
        self.guild_id_ok(ctx, guild_id)?;
        let query = clean_query(ctx.query("query").unwrap_or_default(), LIST_QUERY_MAX);
        if query.is_empty() {
            return Ok(self.json(ctx, 200, json!({ "members": [] }), &[]));
        }
        self.within_search_limit(ctx, &session)?;
        self.managed_guild(ctx, &session, guild_id).await?;
        match self.discord.search_members(guild_id, &query).await {
            Ok(members) => Ok(self.json(ctx, 200, json!({ "members": members }), &[])),
            Err(e) => Err(self.discord_failure(ctx, &e, false)),
        }
    }

    /// Answers a failed record operation (`recordFailure`).
    fn record_failure(&self, ctx: &Ctx, err: StoreError, write: bool, what: &str) -> Response {
        match err {
            StoreError::Field(e) => {
                let mut extra = Map::new();
                if !e.path.is_empty() {
                    extra.insert("path".into(), json!(e.path));
                }
                self.fail(ctx, 400, "invalid_request", &e.message, extra, &[])
            }
            StoreError::Record(e) => {
                if e.code.as_str() == "discord" {
                    self.logger.warn(&format!("Dashboard: {what}: {}", e.message));
                }
                let mut extra = Map::new();
                if let Some(p) = e.path.as_deref().filter(|p| !p.is_empty()) {
                    extra.insert("path".into(), json!(p));
                }
                if e.saved {
                    extra.insert("saved".into(), json!(true));
                }
                self.fail(ctx, e.status(), record_error_code(e.code.as_str()), &e.message, extra, &[])
            }
            StoreError::Discord(e) => {
                self.logger.warn(&format!("Dashboard: {what}: {}", e.message));
                self.fail(
                    ctx,
                    502,
                    "discord_failed",
                    "Discord refused the change. Check the bot's permissions and try again.",
                    Map::new(),
                    &[],
                )
            }
            StoreError::Internal(e) => {
                self.logger.error(&format!("Dashboard: {what} failed"), Some(&e.to_string()));
                if write {
                    self.fail(ctx, 500, "save_failed", "The change could not be saved. Try again in a moment.", Map::new(), &[])
                } else {
                    self.fail(ctx, 503, "unavailable", "The list could not be loaded. Try again in a moment.", Map::new(), &[])
                }
            }
        }
    }

    async fn guild_records(&self, ctx: &mut Ctx, guild_id: &str, key: &str, item_id: Option<String>, action_id: Option<String>) -> R<Response> {
        let methods: &[&str] = if action_id.is_some() {
            &["POST"]
        } else if item_id.is_some() {
            &["GET", "PATCH", "DELETE"]
        } else {
            &["GET", "POST"]
        };
        self.allow(ctx, methods)?;
        let method = ctx.method.clone();
        let write = method != "GET";
        if write {
            self.check_write(ctx)?;
        }
        let session = self.require_session(ctx)?;
        self.guild_id_ok(ctx, guild_id)?;
        let Some(collection) = self.collections.get(key).cloned() else {
            return Err(self.stop(ctx, 404, "not_found", "Unknown list."));
        };
        if item_id.as_deref().is_some_and(|id| !RECORD_ID_RE.test(id)) {
            return Err(self.stop(ctx, 400, "invalid_request", "That is not a valid item id."));
        }
        if let Some(a) = action_id.as_deref()
            && (!ACTION_ID_RE.test(a) || collection.action(a).is_none())
        {
            return Err(self.stop(ctx, 404, "not_found", "Unknown action."));
        }
        let Some(records) = self.records.clone() else {
            return Err(self.stop(ctx, 503, "unavailable", "Lists cannot be managed right now."));
        };
        let mut data = Value::Null;
        if write {
            self.within_write_limit(ctx, &session)?;
            let parsed = self.read_json(ctx, RECORD_BODY_LIMIT_BYTES, method == "DELETE").await?;
            if method != "DELETE" {
                match parsed {
                    Value::Object(mut o) if o.keys().all(|k| k == "data") => data = o.remove("data").unwrap_or(Value::Null),
                    _ => return Err(self.stop(ctx, 400, "invalid_request", "The body must be { \"data\": { ... } }.")),
                }
            }
        }
        let guild = self.managed_guild(ctx, &session, guild_id).await?;

        let label = format!(
            "{key}{}{}",
            item_id.as_deref().map(|i| format!("/{i}")).unwrap_or_default(),
            action_id.as_deref().map(|a| format!("!{a}")).unwrap_or_default()
        );
        let what = format!("{method} {label} in guild {guild_id}");
        // The acting admin, for the stores' placeholders ({user.*}) and ping rules.
        let actor = Actor {
            user_id: session.uid.clone(),
            user: Some(DashboardUser {
                id: session.uid.clone(),
                username: Some(session.username.clone()),
                global_name: session.global_name.clone(),
                avatar: session.avatar.clone(),
            }),
            member: Some(DashboardMember { owner: guild.owner, permissions: guild.permissions.clone(), highest_role_position: None }),
        };
        let result = self
            .records_op(ctx, &records, &session, guild_id, key, &collection, item_id.as_deref(), action_id.as_deref(), data, &actor, &label)
            .await;
        match result {
            Ok(res) => Ok(res),
            Err(RecordStop::Stop(s)) => Err(s),
            Err(RecordStop::Store(e)) => Ok(self.record_failure(ctx, e, write, &what)),
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn records_op(
        &self,
        ctx: &mut Ctx,
        records: &Arc<dyn RecordsBridge>,
        session: &Session,
        guild_id: &str,
        key: &str,
        collection: &Collection,
        item_id: Option<&str>,
        action_id: Option<&str>,
        data: Value,
        actor: &Actor,
        label: &str,
    ) -> Result<Response, RecordStop> {
        let method = ctx.method.clone();
        if method == "GET" && item_id.is_none() {
            // Searchable collections take ?query= (what to look for) and ?cursor= (the page after a previous answer).
            let query = clean_query(ctx.query("query").unwrap_or_default(), LIST_QUERY_MAX);
            // A search may ask Discord (member names), so it shares the member search's per-user limit.
            if !query.is_empty() {
                self.within_search_limit(ctx, session).map_err(RecordStop::Stop)?;
            }
            let raw_cursor = ctx.query("cursor").unwrap_or_default();
            let cursor = if LIST_CURSOR_RE.test(raw_cursor) { raw_cursor.to_string() } else { String::new() };
            let result = records.list(guild_id, key, ListOpts { query, cursor }).await?;
            let mut payload = Map::new();
            payload.insert("items".into(), Value::Array(result.items));
            if let Some(limit) = collection.get("limit") {
                payload.insert("limit".into(), limit.clone());
            }
            if let Some(next) = result.next_cursor.filter(|c| LIST_CURSOR_RE.test(c)) {
                payload.insert("nextCursor".into(), json!(next));
            }
            return Ok(self.json(ctx, 200, Value::Object(payload), &[]));
        }
        let not_found = || format!("That {} does not exist any more.", collection.singular());
        if method == "GET" {
            let item = records.get(guild_id, key, item_id.unwrap_or_default()).await?;
            return Ok(match item.filter(|i| !i.is_null()) {
                None => self.fail(ctx, 404, "not_found", &not_found(), Map::new(), &[]),
                Some(item) => self.json(ctx, 200, json!({ "item": item }), &[]),
            });
        }
        if method == "DELETE" {
            records.remove(guild_id, key, item_id.unwrap_or_default(), actor).await?;
            self.logger.info(&format!("Dashboard: user {} deleted {label} in guild {guild_id}", session.uid));
            return Ok(self.send(ctx, 204, None, &[("Cache-Control", "no-store".to_string())]));
        }

        let mut current: Option<Value> = None;
        if let Some(id) = item_id {
            match records.get(guild_id, key, id).await?.filter(|i| !i.is_null()) {
                Some(c) => current = Some(c),
                None => return Ok(self.fail(ctx, 404, "not_found", &not_found(), Map::new(), &[])),
            }
        }
        let mode = if action_id.is_some() {
            Mode::Action
        } else if item_id.is_some() {
            Mode::Update
        } else {
            Mode::Create
        };
        let valid = self.collections.validate_record_input(key, &data, mode, action_id, current.as_ref(), self.now())?;
        let (keep, keep_keys) = match &current {
            Some(c) => {
                let (refs, keys) = collection.record_refs(Some(c));
                (refs.to_keep(), keys)
            }
            None => (KeepRefs::default(), IndexSet::new()),
        };
        let valid_data = valid.data.clone();
        let key_of = |r: &RoleRef| collection.ref_key_for(r, &valid_data);
        self.verify_refs(ctx, session, guild_id, &valid.refs, &keep, &keep_keys, &key_of).await.map_err(RecordStop::Stop)?;
        if mode == Mode::Update && collection.check_stored_roles() {
            let mut saved = current.as_ref().and_then(Value::as_object).cloned().unwrap_or_default();
            saved.extend(valid.data.clone());
            self.verify_stored_roles(ctx, session, guild_id, collection, &Value::Object(saved), &valid.data)
                .await
                .map_err(RecordStop::Stop)?;
        }

        match mode {
            Mode::Create => {
                let saved = records.create(guild_id, key, valid.data, actor).await?;
                let id = match saved.item.get("id") {
                    None | Some(Value::Null) => "?".to_string(),
                    Some(v) => dash_schema::js::to_js_string(Some(v)),
                };
                self.logger.info(&format!("Dashboard: user {} created {key}/{id} in guild {guild_id}", session.uid));
                Ok(self.json(ctx, 201, item_with_message(saved.item, saved.message), &[]))
            }
            Mode::Update => {
                let names: Vec<String> = dash_schema::js::js_keys(&valid.data).into_iter().cloned().collect();
                let saved = records.update(guild_id, key, item_id.unwrap_or_default(), valid.data, actor).await?;
                self.logger.info(&format!("Dashboard: user {} changed {label} in guild {guild_id}: {}", session.uid, names.join(", ")));
                Ok(self.json(ctx, 200, item_with_message(saved.item, saved.message), &[]))
            }
            Mode::Action => {
                let answer =
                    records.action(guild_id, key, item_id.unwrap_or_default(), action_id.unwrap_or_default(), valid.data, actor).await?;
                self.logger.info(&format!("Dashboard: user {} ran {label} in guild {guild_id}", session.uid));
                // `typeof result?.message === 'string'`: an empty message is sent too.
                let mut out = Map::new();
                out.insert("item".into(), answer.item.unwrap_or(Value::Null));
                if let Some(m) = answer.message {
                    out.insert("message".into(), json!(m));
                }
                Ok(self.json(ctx, 200, Value::Object(out), &[]))
            }
        }
    }

    // ── Static files ────────────────────────────────────────────────────────────

    fn read_changelog(&self) -> String {
        match &self.changelog_path {
            None => crate::static_files::EMBEDDED_CHANGELOG.to_string(),
            Some(p) => std::fs::read(p).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default(),
        }
    }

    /// One static file, loaded (and for the pages rendered) on first request and kept in memory (`staticEntry`).
    fn static_entry(&self, file: &str, content_type: &str) -> Result<Arc<StaticEntry>, LoadError> {
        if let Some(e) = self.static_entries.lock().get(file) {
            return Ok(e.clone());
        }
        let raw = self.public_dir.read(file)?;
        let body = match file {
            "privacy.html" | "terms.html" => {
                // The legal text names the contact itself (contactHtml), so the footer leaves out its "Contact" link.
                let text = String::from_utf8_lossy(&raw).replace(CONTACT_MARKER, &contact_html(self.config.contact.as_ref()));
                let opts = PageOptions { invite_url: Some(self.discord.invite_url(None)), contact: None, support_url: self.config.support_url.clone(), changelog: None };
                render_legal(&text, &opts).map_err(LoadError::Other)?.into_bytes()
            }
            "landing.html" | "docs.html" | "changelog.html" => {
                // The public pages are filled from the catalog, the command list, CHANGELOG.md and the bot's client id.
                let opts = PageOptions {
                    invite_url: Some(self.discord.invite_url(None)),
                    contact: self.config.contact.clone(),
                    support_url: self.config.support_url.clone(),
                    changelog: Some(self.read_changelog()),
                };
                let text = String::from_utf8_lossy(&raw);
                let rendered = match file {
                    "landing.html" => render_landing(&text, &opts),
                    "docs.html" => render_docs(&text, &opts),
                    _ => render_changelog(&text, &opts),
                };
                rendered.map_err(LoadError::Other)?.into_bytes()
            }
            _ => raw,
        };
        let entry = Arc::new(make_entry(body, content_type));
        self.static_entries.lock().insert(file.to_string(), entry.clone());
        Ok(entry)
    }

    async fn static_file(&self, ctx: &mut Ctx, file: &str, content_type: &str) -> R<Response> {
        self.allow(ctx, &["GET", "HEAD"])?;
        let entry = match self.static_entry(file, content_type) {
            Ok(e) => e,
            Err(err) => {
                if let LoadError::Other(msg) = err {
                    self.logger.warn(&format!("Dashboard: could not read {file}: {msg}"));
                }
                return Err(self.stop(ctx, 404, "not_found", "Not found."));
            }
        };
        let zipped = entry.gzip.is_some() && accepts_gzip(ctx.header("accept-encoding").as_deref());
        let mut headers =
            vec![("Cache-Control", "no-cache".to_string()), ("ETag", if zipped { entry.gzip_etag.clone() } else { entry.etag.clone() })];
        if entry.gzip.is_some() {
            headers.push(("Vary", "Accept-Encoding".to_string()));
        }
        if etag_matches(ctx.header("if-none-match").as_deref(), &[&entry.etag, &entry.gzip_etag]) {
            return Ok(self.send(ctx, 304, None, &headers));
        }
        if zipped {
            headers.push(("Content-Encoding", "gzip".to_string()));
        }
        let mut all = vec![("Content-Type", content_type.to_string())];
        all.extend(headers);
        let body = if zipped { entry.gzip.clone().unwrap_or_default() } else { entry.body.clone() };
        Ok(self.send(ctx, 200, Some(body), &all))
    }
}

/// Inside the records route: a route answer (Stop) or a store failure that recordFailure maps.
enum RecordStop {
    Stop(Stop),
    Store(StoreError),
}

impl From<StoreError> for RecordStop {
    fn from(e: StoreError) -> Self {
        Self::Store(e)
    }
}

fn item_with_message(item: Value, message: Option<String>) -> Value {
    let mut out = Map::new();
    out.insert("item".into(), item);
    if let Some(m) = message.filter(|m| !m.is_empty()) {
        out.insert("message".into(), json!(m));
    }
    Value::Object(out)
}

/// RecordError code -> the `error` of the answer (`RECORD_ERROR_CODES`).
fn record_error_code(code: &str) -> &'static str {
    match code {
        "invalid" => "invalid_request",
        "not_found" => "not_found",
        "conflict" => "conflict",
        "limit" => "limit",
        "discord" => "discord_failed",
        "failed" => "action_failed",
        _ => "unavailable",
    }
}

/// The highest position of a member's roles (0 without any): `Math.max(0, ...roles.map(position ?? 0))`.
fn top_position(res: &DiscordResources, roles: &[String]) -> i64 {
    roles.iter().map(|id| res.role(id).map_or(0, |r| r.position)).fold(0, i64::max)
}

/// getGuildResources in the shape schema.checkRefs takes (`refResources`).
fn ref_resources(res: &DiscordResources) -> GuildResources {
    GuildResources {
        channels: res.channels.iter().map(|c| (c.id.clone(), ChannelInfo { kind: c.kind.clone() })).collect(),
        roles: res
            .roles
            .iter()
            .map(|r| (r.id.clone(), RoleInfo { everyone: r.everyone, managed: r.managed, assignable: r.assignable == Some(true) }))
            .collect(),
    }
}

/// Every id of `refs` is known to the server or kept (`knowsAll`).
fn knows_all(refs: &Refs, res: &DiscordResources, keep: &KeepRefs) -> bool {
    refs.channels.iter().all(|r| res.channel(&r.id).is_some() || keep.channels.contains(&r.id))
        && refs.roles.iter().all(|r| res.role(&r.id).is_some() || keep.roles.contains(&r.id))
}

/// The GET /api/guilds/:id/resources payload: getGuildResources without the server-only parts (`publicResources`).
fn public_resources(res: &DiscordResources) -> Value {
    let channels: Vec<Value> = res
        .channels
        .iter()
        .map(|c| {
            json!({ "id": c.id, "name": c.name, "type": c.kind_id, "kind": c.kind, "parentId": c.parent_id, "position": c.position, "nsfw": c.nsfw })
        })
        .collect();
    let roles: Vec<Value> = res
        .roles
        .iter()
        .map(|r| {
            let mut m = Map::new();
            m.insert("id".into(), json!(r.id));
            m.insert("name".into(), json!(r.name));
            m.insert("color".into(), json!(r.color));
            m.insert("position".into(), json!(r.position));
            m.insert("managed".into(), json!(r.managed));
            m.insert("everyone".into(), json!(r.everyone));
            if let Some(v) = r.mentionable {
                m.insert("mentionable".into(), json!(v));
            }
            if let Some(v) = r.assignable {
                m.insert("assignable".into(), json!(v));
            }
            Value::Object(m)
        })
        .collect();
    let emojis: Vec<Value> = res.emojis.iter().map(|e| json!({ "id": e.id, "name": e.name, "animated": e.animated })).collect();
    json!({
        "channels": channels, "roles": roles, "emojis": emojis,
        "bot": { "userId": res.bot.user_id, "topRolePosition": res.bot.top_role_position },
    })
}
