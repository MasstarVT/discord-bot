//! A recording, scriptable Discord REST double for `app.http()`.
//!
//! [`FakeRest`] runs a tiny HTTP/1.1 server on a loopback port (in its own thread, so it works with any test
//! runtime). serenity's [`serenity::all::HttpBuilder::proxy`] sends every REST call there instead of
//! `https://discord.com`, so feature code runs its real serenity calls (`channel.send_message`, `guild.ban`,
//! `create_dm_channel`, raw `Request`s) and the test asserts on what was sent:
//!
//! ```ignore
//! let rest = FakeRest::new();
//! let t = TestApp::new().with_fake_guilds_rest(&[guild], &rest);
//! rest.on("GET", "/channels/*", 200, channel_json);         // script a route (`*` = one path segment)
//! rest.fail("PUT", "/guilds/*/bans/*", 403, 50013, "Missing Permissions");
//! run_the_feature(&t).await;
//! let sends = rest.calls_to("POST", "/channels/123/messages");
//! assert_eq!(sends[0].body["content"], "hi");
//! ```
//!
//! Unscripted routes get defaults that keep common flows working: a message create / edit echoes a message (author
//! [`super::TEST_BOT_ID`]), `POST /users/@me/channels` opens a DM channel, `GET` of a channel, message, member or
//! role answers Discord's 404 (`Unknown Channel` 10003, `Unknown Message` 10008, `Unknown Member` 10007, `Unknown
//! Role` 10011), and everything else answers `204 No Content`.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

/// One REST call the double received.
#[derive(Debug, Clone, PartialEq)]
pub struct RestCall {
    /// `GET`, `POST`, `PUT`, `PATCH`, `DELETE`.
    pub method: String,
    /// The route without `/api/v10` and without the query, e.g. `/channels/1/messages`.
    pub path: String,
    /// The decoded query string pairs, in order.
    pub query: Vec<(String, String)>,
    /// The decoded `X-Audit-Log-Reason` header.
    pub reason: Option<String>,
    /// The JSON body (for a multipart request, its `payload_json` part); `Null` when there is none.
    pub body: Value,
    /// The file names of a multipart request's file parts.
    pub files: Vec<String>,
    /// Every request header, lower-cased names.
    pub headers: HashMap<String, String>,
}

impl RestCall {
    /// A query parameter.
    pub fn query_param(&self, key: &str) -> Option<&str> {
        self.query.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }
}

/// A scripted answer.
type Responder = Arc<dyn Fn(&RestCall) -> (u16, Value) + Send + Sync>;

struct Script {
    method: String,
    pattern: String,
    respond: Responder,
    /// How many more times it answers (`None`: always).
    remaining: Option<usize>,
}

#[derive(Default)]
struct Inner {
    calls: Mutex<Vec<RestCall>>,
    scripts: Mutex<Vec<Script>>,
    stop: AtomicBool,
}

/// The REST double. Cheap to clone (shared state); the server stops when the last clone is dropped.
#[derive(Clone)]
pub struct FakeRest {
    inner: Arc<Inner>,
    addr: SocketAddr,
    _guard: Arc<StopGuard>,
}

struct StopGuard {
    inner: Arc<Inner>,
    addr: SocketAddr,
}

impl Drop for StopGuard {
    fn drop(&mut self) {
        self.inner.stop.store(true, Ordering::SeqCst);
        // Wake the accept loop.
        let _ = TcpStream::connect(self.addr);
    }
}

impl std::fmt::Debug for FakeRest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FakeRest").field("addr", &self.addr).finish()
    }
}

impl Default for FakeRest {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeRest {
    /// Starts the server on a free loopback port.
    pub fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("FakeRest binds a loopback port");
        let addr = listener.local_addr().expect("bound address");
        let inner = Arc::new(Inner::default());
        let state = inner.clone();
        std::thread::Builder::new()
            .name("fake-rest".into())
            .spawn(move || {
                for stream in listener.incoming() {
                    if state.stop.load(Ordering::SeqCst) {
                        break;
                    }
                    let Ok(stream) = stream else { continue };
                    let st = state.clone();
                    std::thread::spawn(move || serve(&st, stream));
                }
            })
            .expect("FakeRest thread starts");
        Self { inner: inner.clone(), addr, _guard: Arc::new(StopGuard { inner, addr }) }
    }

    /// `http://127.0.0.1:<port>`.
    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// A serenity HTTP client whose calls all reach this double (rate limiter off).
    pub fn http(&self) -> serenity::all::Http {
        serenity::all::HttpBuilder::new("fake-rest-token").proxy(self.base_url()).ratelimiter_disabled(true).build()
    }

    /// A Discord handle with these guilds cached, the bot user ([`super::TEST_BOT_ID`]) known, and REST going here.
    pub fn discord(&self, guilds: &[Value]) -> crate::discord::DiscordHandle {
        super::offline_discord_as_with(guilds, self.http())
    }

    /// Scripts `method pattern` (`*` matches one path segment, a trailing `**` any rest) to answer `status` with
    /// `body` (`Null`: no body). The newest matching script wins.
    pub fn on(&self, method: &str, pattern: &str, status: u16, body: Value) -> &Self {
        self.push(method, pattern, None, Arc::new(move |_| (status, body.clone())))
    }

    /// Like [`Self::on`], answering only the next `times` matching calls.
    pub fn on_times(&self, method: &str, pattern: &str, times: usize, status: u16, body: Value) -> &Self {
        self.push(method, pattern, Some(times), Arc::new(move |_| (status, body.clone())))
    }

    /// Scripts a computed answer.
    pub fn on_with(&self, method: &str, pattern: &str, f: impl Fn(&RestCall) -> (u16, Value) + Send + Sync + 'static) -> &Self {
        self.push(method, pattern, None, Arc::new(f))
    }

    /// Scripts a Discord error: HTTP `status` with JSON `{ code, message }` (serenity's `UnsuccessfulRequest`, so
    /// [`crate::discord::error_code`] reads `code`).
    pub fn fail(&self, method: &str, pattern: &str, status: u16, code: u64, message: &str) -> &Self {
        let body = json!({ "code": code, "message": message });
        self.push(method, pattern, None, Arc::new(move |_| (status, body.clone())))
    }

    fn push(&self, method: &str, pattern: &str, remaining: Option<usize>, respond: Responder) -> &Self {
        self.inner.scripts.lock().unwrap_or_else(|e| e.into_inner()).push(Script {
            method: method.to_ascii_uppercase(),
            pattern: pattern.to_string(),
            respond,
            remaining,
        });
        self
    }

    /// Forgets every script (the defaults apply again).
    pub fn clear_scripts(&self) {
        self.inner.scripts.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    /// Every call so far, in arrival order.
    pub fn calls(&self) -> Vec<RestCall> {
        self.inner.calls.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// The calls matching `method pattern` (same pattern rules as [`Self::on`]).
    pub fn calls_to(&self, method: &str, pattern: &str) -> Vec<RestCall> {
        let m = method.to_ascii_uppercase();
        self.calls().into_iter().filter(|c| c.method == m && path_matches(pattern, &c.path)).collect()
    }

    /// Forgets the recorded calls.
    pub fn clear_calls(&self) {
        self.inner.calls.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }
}

/// Whether `path` matches `pattern` (`*` one segment, a trailing `**` the rest).
pub fn path_matches(pattern: &str, path: &str) -> bool {
    let p: Vec<&str> = pattern.trim_matches('/').split('/').collect();
    let s: Vec<&str> = path.trim_matches('/').split('/').collect();
    for (i, seg) in p.iter().enumerate() {
        if *seg == "**" && i == p.len() - 1 {
            return s.len() >= i;
        }
        match s.get(i) {
            Some(x) if *seg == "*" || seg == x => {}
            _ => return false,
        }
    }
    p.len() == s.len()
}

fn percent_decode(s: &str, plus_space: bool) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => match std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok()) {
                Some(v) => {
                    out.push(v);
                    i += 3;
                    continue;
                }
                None => out.push(b'%'),
            },
            b'+' if plus_space => out.push(b' '),
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn serve(state: &Inner, stream: TcpStream) {
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(10)));
    let Ok(write_half) = stream.try_clone() else { return };
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    if reader.read_line(&mut line).unwrap_or(0) == 0 {
        return;
    }
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_ascii_uppercase();
    let target = parts.next().unwrap_or("/").to_string();
    let mut headers = HashMap::new();
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h).unwrap_or(0) == 0 {
            break;
        }
        let h = h.trim_end_matches(['\r', '\n']);
        if h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    let body = read_body(&mut reader, &headers);
    let (path_part, query_part) = target.split_once('?').unwrap_or((target.as_str(), ""));
    let path = path_part.strip_prefix("/api/v10").unwrap_or(path_part).to_string();
    let query = query_part
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (percent_decode(k, true), percent_decode(v, true))
        })
        .collect();
    let reason = headers.get("x-audit-log-reason").map(|r| percent_decode(r, false));
    let (json_body, files) = decode_body(&headers, &body);
    let call = RestCall { method, path, query, reason, body: json_body, files, headers };
    state.calls.lock().unwrap_or_else(|e| e.into_inner()).push(call.clone());
    let (status, answer) = answer(state, &call);
    let text = if answer.is_null() || status == 204 { String::new() } else { answer.to_string() };
    let status = if text.is_empty() && (200..300).contains(&status) { 204 } else { status };
    let response = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
        reason_phrase(status),
        text.len()
    );
    let mut w = write_half;
    let _ = w.write_all(response.as_bytes());
    let _ = w.flush();
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        _ => "Status",
    }
}

fn read_body(reader: &mut BufReader<TcpStream>, headers: &HashMap<String, String>) -> Vec<u8> {
    if headers.get("transfer-encoding").is_some_and(|v| v.to_ascii_lowercase().contains("chunked")) {
        let mut out = Vec::new();
        loop {
            let mut size = String::new();
            if reader.read_line(&mut size).unwrap_or(0) == 0 {
                break;
            }
            let n = usize::from_str_radix(size.trim().split(';').next().unwrap_or("0"), 16).unwrap_or(0);
            if n == 0 {
                let mut end = String::new();
                let _ = reader.read_line(&mut end);
                break;
            }
            let mut chunk = vec![0u8; n];
            if reader.read_exact(&mut chunk).is_err() {
                break;
            }
            out.extend(chunk);
            let mut crlf = [0u8; 2];
            let _ = reader.read_exact(&mut crlf);
        }
        return out;
    }
    let len = headers.get("content-length").and_then(|v| v.parse::<usize>().ok()).unwrap_or(0);
    let mut body = vec![0u8; len];
    if len > 0 && reader.read_exact(&mut body).is_err() {
        body.clear();
    }
    body
}

fn decode_body(headers: &HashMap<String, String>, body: &[u8]) -> (Value, Vec<String>) {
    let ctype = headers.get("content-type").cloned().unwrap_or_default();
    if let Some(boundary) = ctype.split(';').find_map(|p| p.trim().strip_prefix("boundary=")).map(|b| b.trim_matches('"').to_string()) {
        return decode_multipart(body, &boundary);
    }
    if body.is_empty() {
        return (Value::Null, Vec::new());
    }
    (serde_json::from_slice(body).unwrap_or_else(|_| Value::String(String::from_utf8_lossy(body).into_owned())), Vec::new())
}

fn decode_multipart(body: &[u8], boundary: &str) -> (Value, Vec<String>) {
    let text = String::from_utf8_lossy(body);
    let delim = format!("--{boundary}");
    let mut json = Value::Null;
    let mut files = Vec::new();
    for part in text.split(delim.as_str()) {
        let part = part.trim_start_matches("\r\n");
        let Some((head, content)) = part.split_once("\r\n\r\n") else { continue };
        let content = content.strip_suffix("\r\n").unwrap_or(content);
        let disp = head.lines().find(|l| l.to_ascii_lowercase().starts_with("content-disposition")).unwrap_or("");
        let attr = |name: &str| {
            disp.split(';').find_map(|p| p.trim().strip_prefix(&format!("{name}=")).map(|v| v.trim_matches('"').to_string()))
        };
        if let Some(f) = attr("filename") {
            files.push(f);
        } else if attr("name").as_deref() == Some("payload_json") {
            json = serde_json::from_str(content).unwrap_or(Value::Null);
        }
    }
    (json, files)
}

fn answer(state: &Inner, call: &RestCall) -> (u16, Value) {
    let scripted = {
        let mut scripts = state.scripts.lock().unwrap_or_else(|e| e.into_inner());
        let idx = scripts.iter().rposition(|s| s.method == call.method && path_matches(&s.pattern, &call.path) && s.remaining != Some(0));
        idx.map(|i| {
            if let Some(n) = scripts[i].remaining.as_mut() {
                *n -= 1;
            }
            scripts[i].respond.clone()
        })
    };
    if let Some(f) = scripted {
        return f(call);
    }
    default_answer(call)
}

fn not_found(code: u64, message: &str) -> (u16, Value) {
    (404, json!({ "code": code, "message": message }))
}

/// What an unscripted route answers.
pub fn default_answer(call: &RestCall) -> (u16, Value) {
    let seg: Vec<&str> = call.path.trim_matches('/').split('/').collect();
    match (call.method.as_str(), seg.as_slice()) {
        ("POST", ["channels", channel, "messages"]) => (200, echo_message(call, channel, &super::fresh_snowflake().to_string())),
        ("PATCH", ["channels", channel, "messages", id]) => (200, echo_message(call, channel, id)),
        ("POST", ["webhooks", _, _]) | ("POST", ["webhooks", _, _, "messages"]) => {
            (200, echo_message(call, "100000000000000003", &super::fresh_snowflake().to_string()))
        }
        ("POST", ["users", "@me", "channels"]) => {
            let recipient = call.body.get("recipient_id").and_then(|v| v.as_str().map(str::to_string).or_else(|| v.as_u64().map(|n| n.to_string()))).unwrap_or_default();
            (
                200,
                json!({
                    "id": super::fresh_snowflake().to_string(), "type": 1, "last_message_id": null,
                    "recipients": [{ "id": recipient, "username": format!("user{recipient}"), "discriminator": "0", "avatar": null }]
                }),
            )
        }
        ("POST", ["guilds", guild, "roles"]) => (200, echo_role(call, guild, &super::fresh_snowflake().to_string())),
        ("PATCH", ["guilds", guild, "roles", id]) => (200, echo_role(call, guild, id)),
        ("POST", ["guilds", guild, "channels"]) => (200, echo_channel(call, Some(guild), &super::fresh_snowflake().to_string())),
        ("DELETE", ["channels", id]) => (200, echo_channel(call, None, id)),
        ("POST", ["guilds", guild, "scheduled-events"]) => (200, echo_scheduled_event(call, guild, &super::fresh_snowflake().to_string())),
        ("PATCH", ["guilds", guild, "scheduled-events", id]) => (200, echo_scheduled_event(call, guild, id)),
        ("GET", ["channels", _]) => not_found(10003, "Unknown Channel"),
        ("GET", ["channels", _, "messages", _]) => not_found(10008, "Unknown Message"),
        ("GET", ["guilds", _, "members", _]) => not_found(10007, "Unknown Member"),
        ("GET", ["guilds", _, "roles", _]) => not_found(10011, "Unknown Role"),
        ("GET", ["users", _]) => not_found(10013, "Unknown User"),
        ("GET", ["guilds", _, "bans", _]) => not_found(10026, "Unknown Ban"),
        ("GET", ["guilds", _, "bans"]) | ("GET", ["guilds", _, "members"]) | ("GET", ["channels", _, "messages"]) => (200, json!([])),
        _ => (204, Value::Null),
    }
}

/// The message Discord returns for a create / edit with this body.
fn echo_message(call: &RestCall, channel: &str, id: &str) -> Value {
    let mut m = super::message_json(id);
    m["channel_id"] = json!(channel);
    m["timestamp"] = json!(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, false));
    for key in ["content", "embeds", "components", "tts", "flags"] {
        if let Some(v) = call.body.get(key)
            && !v.is_null()
        {
            m[key] = v.clone();
        }
    }
    if let Some(r) = call.body.get("message_reference")
        && r.is_object()
    {
        // Discord answers with a complete reference: the channel is the reply's own when the body leaves it out.
        let mut r = r.clone();
        if r.get("channel_id").is_none_or(Value::is_null) {
            r["channel_id"] = json!(channel);
        }
        m["message_reference"] = r;
    }
    m
}

/// Copies the body's fields that are not null over `base`.
fn overlay(mut base: Value, call: &RestCall, keys: &[&str]) -> Value {
    for key in keys {
        if let Some(v) = call.body.get(*key)
            && !v.is_null()
        {
            base[*key] = v.clone();
        }
    }
    base
}

/// The role Discord returns for a create / edit with this body (`colors` included, which serenity requires).
fn echo_role(call: &RestCall, guild: &str, id: &str) -> Value {
    let mut r = overlay(
        json!({
            "id": id, "guild_id": guild, "name": "new role", "color": 0, "hoist": false, "position": 1, "permissions": "0",
            "managed": false, "mentionable": false, "flags": 0, "icon": null, "unicode_emoji": null,
            "colors": { "primary_color": 0, "secondary_color": null, "tertiary_color": null }
        }),
        call,
        &["name", "color", "hoist", "permissions", "mentionable", "icon", "unicode_emoji", "position"],
    );
    if let Some(c) = call.body.get("color").and_then(Value::as_u64) {
        r["colors"]["primary_color"] = json!(c);
    }
    if let Some(p) = r.get("permissions").and_then(Value::as_u64) {
        r["permissions"] = json!(p.to_string());
    }
    r
}

/// The guild channel Discord returns for a create (or a delete, which answers the deleted channel).
fn echo_channel(call: &RestCall, guild: Option<&str>, id: &str) -> Value {
    let mut c = overlay(
        json!({ "id": id, "type": 0, "name": "channel", "position": 0, "permission_overwrites": [], "nsfw": false, "parent_id": null, "flags": 0 }),
        call,
        &["name", "type", "topic", "position", "parent_id", "nsfw", "user_limit", "bitrate", "rate_limit_per_user", "permission_overwrites"],
    );
    if let Some(g) = guild {
        c["guild_id"] = json!(g);
    }
    if let Some(list) = c.get_mut("permission_overwrites").and_then(Value::as_array_mut) {
        for o in list.iter_mut() {
            for k in ["allow", "deny"] {
                if let Some(n) = o.get(k).and_then(Value::as_u64) {
                    o[k] = json!(n.to_string());
                } else if o.get(k).is_none() {
                    o[k] = json!("0");
                }
            }
        }
    }
    c
}

/// The scheduled event Discord returns for a create / edit with this body.
fn echo_scheduled_event(call: &RestCall, guild: &str, id: &str) -> Value {
    overlay(
        json!({
            "id": id, "guild_id": guild, "channel_id": null, "creator_id": null, "name": "event", "description": null,
            "scheduled_start_time": "2030-01-01T00:00:00.000000+00:00", "scheduled_end_time": null, "privacy_level": 2,
            "status": 1, "entity_type": 3, "entity_id": null, "entity_metadata": null, "user_count": 0, "image": null
        }),
        call,
        &["channel_id", "name", "description", "scheduled_start_time", "scheduled_end_time", "privacy_level", "status", "entity_type", "entity_metadata", "image"],
    )
}
