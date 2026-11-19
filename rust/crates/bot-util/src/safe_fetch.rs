//! Port of `src/utils/safeFetch.js`: the SSRF-safe HTTP client. Every request is checked twice: the URL (scheme, no
//! credentials, IP literals) and every address its host resolves to are validated up front, and the validated
//! addresses are then pinned into the connection (reqwest `resolve_to_addrs`), so the checked address is the one
//! connected to. Redirects are followed by hand and repeat every check. A deadline, a response-size cap counted after
//! decompression, and manual redirect handling bound what a hostile URL can do. Every error is a [`SafeFetchError`].

use crate::image_size::assert_image_dimensions;
use crate::js;
use futures::future::BoxFuture;
use futures::StreamExt;
use indexmap::IndexMap;
use std::io::Write;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, LazyLock};
use std::time::Duration;
use tokio::time::Instant;
use url::Url;

const DEFAULT_TIMEOUT_MS: u64 = 10_000;
const DEFAULT_MAX_BYTES: u64 = 2 * 1024 * 1024;
const DEFAULT_IMAGE_MAX_BYTES: u64 = 8 * 1024 * 1024;

static USER_AGENT: LazyLock<String> = LazyLock::new(|| {
    let contact = std::env::var("HTTP_CONTACT").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "n/a".into());
    format!("DiscordBot/1.0 (+self-hosted; contact: {contact})")
});

/// `DEFAULT_USER_AGENT`: `DiscordBot/1.0 (+self-hosted; contact: <HTTP_CONTACT or n/a>)`.
pub fn default_user_agent() -> &'static str {
    &USER_AGENT
}

// ── Errors ─────────────────────────────────────────────────────────────────────

/// The error codes of `SafeFetchError`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SafeFetchCode {
    InvalidUrl,
    BlockedAddress,
    DnsFailed,
    Timeout,
    TooLarge,
    TooManyRedirects,
    HttpError,
    Network,
    BadContentType,
    BadImage,
}

impl SafeFetchCode {
    /// 'INVALID_URL', 'BLOCKED_ADDRESS', …
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvalidUrl => "INVALID_URL",
            Self::BlockedAddress => "BLOCKED_ADDRESS",
            Self::DnsFailed => "DNS_FAILED",
            Self::Timeout => "TIMEOUT",
            Self::TooLarge => "TOO_LARGE",
            Self::TooManyRedirects => "TOO_MANY_REDIRECTS",
            Self::HttpError => "HTTP_ERROR",
            Self::Network => "NETWORK",
            Self::BadContentType => "BAD_CONTENT_TYPE",
            Self::BadImage => "BAD_IMAGE",
        }
    }
}

impl std::fmt::Display for SafeFetchCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// `SafeFetchError`: a code, a message, and for HTTP errors the status, headers and Retry-After in ms.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("{message}")]
pub struct SafeFetchError {
    pub code: SafeFetchCode,
    pub message: String,
    pub status: Option<u16>,
    pub retry_after_ms: Option<f64>,
    pub headers: Option<IndexMap<String, String>>,
}

impl SafeFetchError {
    /// An error whose message defaults to the code (`new SafeFetchError(code)`).
    pub fn new(code: SafeFetchCode, message: Option<&str>) -> Self {
        Self { code, message: message.unwrap_or(code.as_str()).to_string(), status: None, retry_after_ms: None, headers: None }
    }

    /// `new SafeFetchError('HTTP_ERROR', 404)`: the message becomes 'HTTP 404'.
    pub fn with_status(code: SafeFetchCode, status: u16) -> Self {
        Self { code, message: format!("HTTP {status}"), status: Some(status), retry_after_ms: None, headers: None }
    }

    /// The JS error name.
    pub fn name(&self) -> &'static str {
        "SafeFetchError"
    }
}

fn err(code: SafeFetchCode, message: impl Into<String>) -> SafeFetchError {
    SafeFetchError { code, message: message.into(), status: None, retry_after_ms: None, headers: None }
}

const BLOCKED_MESSAGE: &str = "That address points to a private or reserved network.";

fn timeout_error(timeout_ms: u64) -> SafeFetchError {
    err(SafeFetchCode::Timeout, format!("Request timed out after {timeout_ms} ms."))
}

// ── Address checks ─────────────────────────────────────────────────────────────

const BLOCKED_V4: [([u8; 4], u8); 15] = [
    ([0, 0, 0, 0], 8),
    ([10, 0, 0, 0], 8),
    ([100, 64, 0, 0], 10),
    ([127, 0, 0, 0], 8),
    ([169, 254, 0, 0], 16),
    ([172, 16, 0, 0], 12),
    ([192, 0, 0, 0], 24),
    ([192, 0, 2, 0], 24),
    ([192, 88, 99, 0], 24),
    ([192, 168, 0, 0], 16),
    ([198, 18, 0, 0], 15),
    ([198, 51, 100, 0], 24),
    ([203, 0, 113, 0], 24),
    ([224, 0, 0, 0], 4),
    ([240, 0, 0, 0], 4),
];

const BLOCKED_V6: [([u16; 8], u8); 10] = [
    ([0, 0, 0, 0, 0, 0, 0, 0], 128),
    ([0, 0, 0, 0, 0, 0, 0, 1], 128),
    ([0xfc00, 0, 0, 0, 0, 0, 0, 0], 7),
    ([0xfe80, 0, 0, 0, 0, 0, 0, 0], 10),
    ([0xfec0, 0, 0, 0, 0, 0, 0, 0], 10),
    ([0xff00, 0, 0, 0, 0, 0, 0, 0], 8),
    ([0x2001, 0xdb8, 0, 0, 0, 0, 0, 0], 32),
    ([0x2001, 0, 0, 0, 0, 0, 0, 0], 32),
    ([0x100, 0, 0, 0, 0, 0, 0, 0], 64),
    ([0, 0, 0, 0, 0, 0, 0, 0], 96), // IPv4-compatible (deprecated): ::7f00:1 is ::127.0.0.1
];

fn v4_blocked(a: Ipv4Addr) -> bool {
    let v = u32::from(a);
    BLOCKED_V4.iter().any(|(net, p)| {
        let mask = if *p == 0 { 0 } else { u32::MAX << (32 - p) };
        v & mask == u32::from(Ipv4Addr::from(*net)) & mask
    })
}

fn v6_in_list(a: Ipv6Addr) -> bool {
    let v = u128::from(a);
    BLOCKED_V6.iter().any(|(net, p)| {
        let mask = if *p == 0 { 0 } else { u128::MAX << (128 - p) };
        v & mask == u128::from(Ipv6Addr::from(*net)) & mask
    })
}

fn v4_from(hi: u16, lo: u16) -> Ipv4Addr {
    Ipv4Addr::new((hi >> 8) as u8, (hi & 255) as u8, (lo >> 8) as u8, (lo & 255) as u8)
}

/// Parses an IP literal as Node's `net.isIP` accepts it: strict dotted-quad IPv4 or an IPv6 address.
pub fn parse_ip(s: &str) -> Option<IpAddr> {
    if s.contains('.') && !s.contains(':') {
        return s.parse::<Ipv4Addr>().ok().map(IpAddr::V4);
    }
    s.parse::<Ipv6Addr>().ok().map(IpAddr::V6)
}

/// True when an address must not be connected to: private, loopback, link-local, CGNAT, cloud metadata, multicast,
/// reserved, and IPv6 forms that embed such an IPv4 address. Anything that is not an IP address is blocked.
pub fn is_blocked_address(ip: &str) -> bool {
    let mut s = js::trim(ip);
    if s.starts_with('[') && s.ends_with(']') && s.len() >= 2 {
        s = &s[1..s.len() - 1];
    }
    let s = match s.find('%') {
        Some(i) => &s[..i],
        None => s,
    };
    let s = s.to_lowercase();
    match parse_ip(&s) {
        None => true,
        Some(IpAddr::V4(a)) => v4_blocked(a),
        Some(IpAddr::V6(a)) => {
            if v6_in_list(a) {
                return true;
            }
            let g = a.segments();
            let embedded = if g[..5].iter().all(|x| *x == 0) && g[5] == 0xffff {
                Some(v4_from(g[6], g[7])) // v4-mapped
            } else if g[..4].iter().all(|x| *x == 0) && g[4] == 0xffff && g[5] == 0 {
                Some(v4_from(g[6], g[7])) // v4-translated (SIIT)
            } else if g[0] == 0x64 && g[1] == 0xff9b && g[2..6].iter().all(|x| *x == 0) {
                Some(v4_from(g[6], g[7])) // NAT64
            } else if g[0] == 0x64 && g[1] == 0xff9b && g[2] == 1 {
                Some(v4_from(g[6], g[7])) // local-use NAT64
            } else if g[0] == 0x2002 {
                Some(v4_from(g[1], g[2])) // 6to4
            } else {
                None
            };
            embedded.is_some_and(v4_blocked)
        }
    }
}

// ── Resolution ─────────────────────────────────────────────────────────────────

/// Resolves a host name to addresses (the DNS step; replaceable for tests).
pub trait Resolver: Send + Sync {
    fn lookup(&self, host: &str) -> BoxFuture<'static, std::io::Result<Vec<IpAddr>>>;
}

/// The system resolver (getaddrinfo, in the order it returns).
pub struct SystemResolver;

impl Resolver for SystemResolver {
    fn lookup(&self, host: &str) -> BoxFuture<'static, std::io::Result<Vec<IpAddr>>> {
        let host = host.to_string();
        Box::pin(async move { Ok(tokio::net::lookup_host((host.as_str(), 0)).await?.map(|a| a.ip()).collect()) })
    }
}

/// The SSRF-safe client. [`SafeFetcher::default`] is what the free functions use; tests build one with their own
/// resolver and a dial map that sends a validated public address to a local test server.
#[derive(Clone)]
pub struct SafeFetcher {
    resolver: Arc<dyn Resolver>,
    dial: Option<Arc<dyn Fn(IpAddr) -> IpAddr + Send + Sync>>,
}

impl Default for SafeFetcher {
    fn default() -> Self {
        Self { resolver: Arc::new(SystemResolver), dial: None }
    }
}

struct Target {
    url: Url,
    addresses: Vec<IpAddr>,
}

fn strip_brackets(h: &str) -> &str {
    if h.starts_with('[') && h.ends_with(']') && h.len() >= 2 { &h[1..h.len() - 1] } else { h }
}

/// Options of [`SafeFetcher::fetch`].
#[derive(Debug, Clone)]
pub struct FetchOptions {
    pub method: String,
    /// Header names are case-insensitive (lower-cased, later wins); `host` is ignored.
    pub headers: Vec<(String, String)>,
    pub body: Option<Body>,
    pub timeout_ms: u64,
    pub max_bytes: u64,
    pub max_redirects: u32,
}

impl Default for FetchOptions {
    fn default() -> Self {
        Self { method: "GET".into(), headers: Vec::new(), body: None, timeout_ms: DEFAULT_TIMEOUT_MS, max_bytes: DEFAULT_MAX_BYTES, max_redirects: 5 }
    }
}

/// A request body.
#[derive(Debug, Clone)]
pub enum Body {
    Text(String),
    Bytes(Vec<u8>),
    /// Sent as JSON, with `content-type: application/json` unless one is given.
    Json(serde_json::Value),
}

/// A response.
#[derive(Debug, Clone)]
pub struct FetchResponse {
    pub ok: bool,
    pub status: u16,
    pub status_text: String,
    /// Lower-case names; repeated headers joined with ', ' (Node keeps only the first of some, see `flatten_headers`).
    pub headers: IndexMap<String, String>,
    /// The final URL after redirects.
    pub url: String,
    /// The decompressed body.
    pub buffer: Vec<u8>,
}

impl FetchResponse {
    /// The body as text, decoded with the content-type's charset (UTF-8 by default), as `TextDecoder` does.
    pub fn text(&self) -> String {
        decode_text(&self.buffer, self.headers.get("content-type").map(String::as_str))
    }

    /// The body parsed as JSON.
    pub fn json(&self) -> Result<serde_json::Value, serde_json::Error> {
        serde_json::from_str(&self.text())
    }
}

static CHARSET_RE: LazyLock<crate::jsre::JsRegex> = LazyLock::new(|| crate::jsre::JsRegex::new(r#"charset\s*=\s*"?([^;"\s]+)"#, "i"));

/// `new TextDecoder(label).decode(buffer)` with the content-type's charset; an unknown label is UTF-8.
pub fn decode_text(buffer: &[u8], content_type: Option<&str>) -> String {
    let label = content_type.and_then(|ct| CHARSET_RE.exec(ct)).and_then(|(t, m)| m.group(&t, 1)).unwrap_or_else(|| "utf-8".into());
    let enc = encoding_rs::Encoding::for_label(label.as_bytes())
        .filter(|e| *e != encoding_rs::REPLACEMENT)
        .unwrap_or(encoding_rs::UTF_8);
    // TextDecoder strips a BOM of its own encoding only.
    let (text, _) = enc.decode_without_bom_handling(strip_own_bom(enc, buffer));
    text.into_owned()
}

fn strip_own_bom<'a>(enc: &'static encoding_rs::Encoding, b: &'a [u8]) -> &'a [u8] {
    if enc == encoding_rs::UTF_8 && b.starts_with(&[0xef, 0xbb, 0xbf]) {
        &b[3..]
    } else if enc == encoding_rs::UTF_16LE && b.starts_with(&[0xff, 0xfe]) {
        &b[2..]
    } else if enc == encoding_rs::UTF_16BE && b.starts_with(&[0xfe, 0xff]) {
        &b[2..]
    } else {
        b
    }
}

// Node keeps only the first value of these headers (http.IncomingMessage), and joins the others with ', '.
const SINGLE_VALUE_HEADERS: [&str; 17] = [
    "age",
    "authorization",
    "content-length",
    "content-type",
    "etag",
    "expires",
    "from",
    "host",
    "if-modified-since",
    "if-unmodified-since",
    "last-modified",
    "location",
    "max-forwards",
    "proxy-authorization",
    "referer",
    "retry-after",
    "server",
];

fn flatten_headers(h: &reqwest::header::HeaderMap) -> IndexMap<String, String> {
    let mut out: IndexMap<String, String> = IndexMap::new();
    for (k, v) in h {
        let name = k.as_str().to_ascii_lowercase();
        let value = String::from_utf8_lossy(v.as_bytes()).to_string();
        match out.get_mut(&name) {
            Some(existing) => {
                if !SINGLE_VALUE_HEADERS.contains(&name.as_str()) {
                    existing.push_str(", ");
                    existing.push_str(&value);
                }
            }
            None => {
                out.insert(name, value);
            }
        }
    }
    out
}

/// `Retry-After` as milliseconds: whole seconds, or an HTTP date from now (never negative).
pub fn parse_retry_after(h: Option<&str>) -> Option<f64> {
    let s = js::trim(h?);
    if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) {
        return s.parse::<f64>().ok().map(|n| n * 1000.0);
    }
    let date = js::date_parse(s);
    if date.is_nan() { None } else { Some((date - js::now_ms()).max(0.0)) }
}

fn http_error(res: &FetchResponse) -> SafeFetchError {
    SafeFetchError {
        code: SafeFetchCode::HttpError,
        message: format!("HTTP {}", res.status),
        status: Some(res.status),
        retry_after_ms: if res.status == 429 || res.status == 503 { parse_retry_after(res.headers.get("retry-after").map(String::as_str)) } else { None },
        headers: Some(res.headers.clone()),
    }
}

/// The Node error code of a network failure ('ECONNREFUSED', …), else the error text.
fn network_code(e: &(dyn std::error::Error + 'static)) -> String {
    let mut cur: Option<&(dyn std::error::Error + 'static)> = Some(e);
    while let Some(x) = cur {
        if let Some(io) = x.downcast_ref::<std::io::Error>() {
            use std::io::ErrorKind as K;
            let code = match io.kind() {
                K::ConnectionRefused => "ECONNREFUSED",
                K::ConnectionReset => "ECONNRESET",
                K::ConnectionAborted => "ECONNABORTED",
                K::TimedOut => "ETIMEDOUT",
                K::HostUnreachable => "EHOSTUNREACH",
                K::NetworkUnreachable => "ENETUNREACH",
                K::BrokenPipe => "EPIPE",
                K::AddrNotAvailable => "EADDRNOTAVAIL",
                K::UnexpectedEof => "ECONNRESET",
                _ => "",
            };
            if !code.is_empty() {
                return code.into();
            }
        }
        cur = x.source();
    }
    let mut text = e.to_string();
    let mut src = e.source();
    while let Some(s) = src {
        text = s.to_string();
        src = s.source();
    }
    text
}

/// A sink that refuses to grow past `max` bytes (the decompressed-size cap).
struct Capped {
    data: Vec<u8>,
    max: u64,
    over: bool,
}

impl Write for Capped {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.data.len() as u64 + buf.len() as u64 > self.max {
            self.over = true;
            return Err(std::io::Error::other("TOO_LARGE"));
        }
        self.data.extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

enum Decoder {
    Plain(Capped),
    Gzip(flate2::write::MultiGzDecoder<Capped>),
    Deflate(flate2::write::ZlibDecoder<Capped>),
    Brotli(Box<brotli_decompressor::DecompressorWriter<Capped>>),
}

impl Decoder {
    fn new(encoding: &str, max: u64) -> Self {
        let sink = Capped { data: Vec::new(), max, over: false };
        match encoding {
            "gzip" | "x-gzip" => Self::Gzip(flate2::write::MultiGzDecoder::new(sink)),
            "deflate" => Self::Deflate(flate2::write::ZlibDecoder::new(sink)),
            "br" => Self::Brotli(Box::new(brotli_decompressor::DecompressorWriter::new(sink, 16 * 1024))),
            _ => Self::Plain(sink),
        }
    }

    fn over(&self) -> bool {
        match self {
            Self::Plain(s) => s.over,
            Self::Gzip(d) => d.get_ref().over,
            Self::Deflate(d) => d.get_ref().over,
            Self::Brotli(d) => d.get_ref().over,
        }
    }

    fn write(&mut self, chunk: &[u8]) -> std::io::Result<()> {
        match self {
            Self::Plain(s) => s.write_all(chunk),
            Self::Gzip(d) => d.write_all(chunk),
            Self::Deflate(d) => d.write_all(chunk),
            Self::Brotli(d) => d.write_all(chunk),
        }
    }

    fn kind(&self) -> &'static str {
        match self {
            Self::Brotli(_) => "br",
            _ => "zlib",
        }
    }

    fn finish(self) -> Result<Capped, (bool, &'static str)> {
        match self {
            Self::Plain(s) => Ok(s),
            Self::Gzip(d) => d.finish().map_err(|_| (false, "Z_BUF_ERROR")),
            Self::Deflate(d) => d.finish().map_err(|_| (false, "Z_BUF_ERROR")),
            Self::Brotli(mut d) => {
                if d.close().is_err() {
                    return Err((d.get_ref().over, "ERR__ERROR_FORMAT_PADDING_1"));
                }
                d.into_inner().map_err(|_| (false, "ERR__ERROR_FORMAT_PADDING_1"))
            }
        }
    }
}

impl SafeFetcher {
    /// A fetcher with its own resolver (tests).
    pub fn with_resolver(resolver: Arc<dyn Resolver>) -> Self {
        Self { resolver, dial: None }
    }

    /// Connects to `map(validated address)` instead of the validated address itself. Only for tests that point a
    /// public address at a local server; every check still runs on the validated address.
    #[doc(hidden)]
    pub fn with_dial_map(mut self, map: Arc<dyn Fn(IpAddr) -> IpAddr + Send + Sync>) -> Self {
        self.dial = Some(map);
        self
    }

    // Parses and validates a URL and resolves its host. The scheme, credential and IP-literal checks run before any
    // DNS call. Returns the URL and the validated addresses, IPv4 first.
    async fn resolve_public(&self, input: &str, deadline: Instant, timeout_ms: u64) -> Result<Target, SafeFetchError> {
        let u = Url::parse(input).map_err(|_| err(SafeFetchCode::InvalidUrl, "That is not a valid URL."))?;
        self.resolve_url(u, deadline, timeout_ms).await
    }

    async fn resolve_url(&self, u: Url, deadline: Instant, timeout_ms: u64) -> Result<Target, SafeFetchError> {
        if u.scheme() != "http" && u.scheme() != "https" {
            return Err(err(SafeFetchCode::InvalidUrl, "Only http and https URLs are allowed."));
        }
        if !u.username().is_empty() || u.password().is_some_and(|p| !p.is_empty()) {
            return Err(err(SafeFetchCode::InvalidUrl, "URLs with a username or password are not allowed."));
        }
        let host = strip_brackets(u.host_str().unwrap_or("")).to_string();
        if host.is_empty() || js::utf16_len(&host) > 253 {
            return Err(err(SafeFetchCode::InvalidUrl, "That is not a valid URL."));
        }
        if let Some(ip) = parse_ip(&host) {
            if is_blocked_address(&host) {
                return Err(err(SafeFetchCode::BlockedAddress, BLOCKED_MESSAGE));
            }
            return Ok(Target { url: u, addresses: vec![ip] });
        }
        let now = Instant::now();
        if now >= deadline {
            return Err(timeout_error(timeout_ms));
        }
        let found = match tokio::time::timeout(deadline - now, self.resolver.lookup(&host)).await {
            Err(_) => return Err(timeout_error(timeout_ms)),
            Ok(Err(_)) => return Err(err(SafeFetchCode::DnsFailed, format!("Could not resolve {host}."))),
            Ok(Ok(list)) => list,
        };
        if found.is_empty() {
            return Err(err(SafeFetchCode::DnsFailed, format!("Could not resolve {host}.")));
        }
        if found.iter().any(|a| is_blocked_address(&a.to_string())) {
            return Err(err(SafeFetchCode::BlockedAddress, BLOCKED_MESSAGE));
        }
        let mut addresses: Vec<IpAddr> = found.iter().filter(|a| a.is_ipv4()).copied().collect();
        addresses.extend(found.iter().filter(|a| !a.is_ipv4()));
        Ok(Target { url: u, addresses })
    }

    /// Parses a URL and resolves its host, refusing anything that is not a public http(s) address.
    pub async fn assert_public_url(&self, url: &str) -> Result<Url, SafeFetchError> {
        let deadline = Instant::now() + Duration::from_millis(DEFAULT_TIMEOUT_MS);
        Ok(self.resolve_public(url, deadline, DEFAULT_TIMEOUT_MS).await?.url)
    }

    fn client_for(&self, target: &Target) -> Result<reqwest::Client, SafeFetchError> {
        let mut b = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .no_gzip()
            .no_brotli()
            .no_deflate()
            .http1_only();
        if let Some(host) = target.url.host_str()
            && parse_ip(strip_brackets(host)).is_none() {
                // Every validated address is re-checked here, so a connection can never reach a blocked one.
                let addrs: Vec<SocketAddr> = target
                    .addresses
                    .iter()
                    .filter(|a| !is_blocked_address(&a.to_string()))
                    .map(|a| SocketAddr::new(self.dial.as_ref().map_or(*a, |d| d(*a)), 0))
                    .collect();
                if addrs.is_empty() {
                    return Err(err(SafeFetchCode::BlockedAddress, BLOCKED_MESSAGE));
                }
                b = b.resolve_to_addrs(host, &addrs);
            }
        b.build().map_err(|e| err(SafeFetchCode::Network, format!("Network error: {}", network_code(&e))))
    }

    /// Fetches a public http(s) URL with SSRF protection, a timeout and a size cap. Redirects are followed by hand
    /// (up to `max_redirects`) and each hop repeats the URL and address checks. A 304 comes back as
    /// `ok: false, status: 304` for conditional GETs.
    pub async fn fetch(&self, url: &str, opts: &FetchOptions) -> Result<FetchResponse, SafeFetchError> {
        let timeout_ms = opts.timeout_ms;
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        match tokio::time::timeout_at(deadline, self.fetch_inner(url, opts, deadline)).await {
            Ok(r) => r,
            Err(_) => Err(timeout_error(timeout_ms)),
        }
    }

    async fn fetch_inner(&self, url: &str, opts: &FetchOptions, deadline: Instant) -> Result<FetchResponse, SafeFetchError> {
        let timeout_ms = opts.timeout_ms;
        let mut verb = opts.method.to_uppercase();
        let mut hdrs: IndexMap<String, String> = IndexMap::new();
        hdrs.insert("user-agent".into(), default_user_agent().into());
        hdrs.insert("accept".into(), "*/*".into());
        hdrs.insert("accept-encoding".into(), "gzip, deflate, br".into());
        for (k, v) in &opts.headers {
            hdrs.insert(k.to_lowercase(), v.clone());
        }
        hdrs.shift_remove("host");

        let mut payload: Option<Vec<u8>> = None;
        if let Some(body) = &opts.body
            && verb != "GET" && verb != "HEAD" {
                let bytes = match body {
                    Body::Text(s) => s.clone().into_bytes(),
                    Body::Bytes(b) => b.clone(),
                    Body::Json(v) => {
                        hdrs.entry("content-type".into()).or_insert_with(|| "application/json".into());
                        serde_json::to_vec(v).unwrap_or_default()
                    }
                };
                hdrs.insert("content-length".into(), bytes.len().to_string());
                payload = Some(bytes);
            }

        let mut hop = 0;
        let mut target = self.resolve_public(url, deadline, timeout_ms).await?;
        loop {
            let client = self.client_for(&target)?;
            let method = reqwest::Method::from_bytes(verb.as_bytes()).map_err(|_| err(SafeFetchCode::Network, "Network error: ERR_INVALID_HTTP_TOKEN"))?;
            let mut req = client.request(method, target.url.clone());
            for (k, v) in &hdrs {
                req = req.header(k.as_str(), v.as_str());
            }
            if let Some(p) = &payload {
                req = req.body(p.clone());
            }
            let res = req.send().await.map_err(|e| err(SafeFetchCode::Network, format!("Network error: {}", network_code(&e))))?;
            let status = res.status().as_u16();
            let location = res.headers().get("location").map(|v| String::from_utf8_lossy(v.as_bytes()).to_string());

            if matches!(status, 301 | 302 | 303 | 307 | 308)
                && let Some(location) = location.filter(|l| !l.is_empty()) {
                    drop(res);
                    hop += 1;
                    if hop > opts.max_redirects {
                        return Err(err(SafeFetchCode::TooManyRedirects, "Too many redirects."));
                    }
                    let next = target.url.join(&location).map_err(|_| err(SafeFetchCode::InvalidUrl, "The server sent an invalid redirect."))?;
                    if status == 303 || ((status == 301 || status == 302) && verb == "POST") {
                        verb = "GET".into();
                        payload = None;
                        hdrs.shift_remove("content-type");
                        hdrs.shift_remove("content-length");
                    }
                    if next.origin() != target.url.origin() {
                        hdrs.shift_remove("authorization");
                        hdrs.shift_remove("cookie");
                    }
                    target = self.resolve_url(next, deadline, timeout_ms).await?;
                    continue;
                }

            let flat = flatten_headers(res.headers());
            if let Some(declared) = flat.get("content-length").and_then(|v| js::trim(v).parse::<f64>().ok())
                && declared > opts.max_bytes as f64 {
                    return Err(err(SafeFetchCode::TooLarge, format!("The response is larger than {} bytes.", opts.max_bytes)));
                }
            let status_text = res
                .extensions()
                .get::<hyper::ext::ReasonPhrase>()
                .map(|r| String::from_utf8_lossy(r.as_bytes()).to_string())
                .unwrap_or_else(|| res.status().canonical_reason().unwrap_or("").to_string());
            let encoding = js::trim(flat.get("content-encoding").map(String::as_str).unwrap_or("")).to_lowercase();
            let buffer = if verb == "HEAD" { Vec::new() } else { read_body(res, &encoding, opts.max_bytes, deadline, timeout_ms).await? };
            return Ok(FetchResponse {
                ok: (200..300).contains(&status),
                status,
                status_text,
                headers: flat,
                url: target.url.to_string(),
                buffer,
            });
        }
    }

    /// GET (or any method) and parse JSON; `accept: application/json` unless given. A non-2xx status or a body that
    /// is not JSON is an HTTP_ERROR (with status, headers and Retry-After).
    pub async fn fetch_json(&self, url: &str, opts: &FetchOptions) -> Result<serde_json::Value, SafeFetchError> {
        let res = self.fetch(url, &with_default_header(opts, "accept", "application/json")).await?;
        if !res.ok {
            return Err(http_error(&res));
        }
        res.json().map_err(|_| SafeFetchError {
            code: SafeFetchCode::HttpError,
            message: "Response is not valid JSON".into(),
            status: Some(res.status),
            retry_after_ms: None,
            headers: None,
        })
    }

    /// The body as text; a non-2xx status is an HTTP_ERROR.
    pub async fn fetch_text(&self, url: &str, opts: &FetchOptions) -> Result<String, SafeFetchError> {
        let res = self.fetch(url, opts).await?;
        if !res.ok {
            return Err(http_error(&res));
        }
        Ok(res.text())
    }

    /// Downloads an image: the content type must be image/* (not SVG) and the header must declare a sane size
    /// (`max_side`/`max_pixels`, default 8000 and 16,000,000). Default cap 8 MiB and 8 s.
    pub async fn fetch_image(&self, url: &str, opts: &ImageOptions) -> Result<Vec<u8>, SafeFetchError> {
        let base = FetchOptions { headers: opts.headers.clone(), max_bytes: opts.max_bytes, timeout_ms: opts.timeout_ms, ..FetchOptions::default() };
        let res = self.fetch(url, &with_default_header(&base, "accept", "image/*")).await?;
        if !res.ok {
            return Err(http_error(&res));
        }
        let ct = res.headers.get("content-type").cloned().unwrap_or_default();
        let kind = js::trim(ct.split(';').next().unwrap_or("")).to_lowercase();
        if !kind.starts_with("image/") || kind == "image/svg+xml" {
            return Err(err(SafeFetchCode::BadContentType, "That URL is not an image."));
        }
        let max_side = opts.max_side.filter(|v| *v > 0).unwrap_or(crate::image_size::DEFAULT_MAX_SIDE);
        let max_pixels = opts.max_pixels.filter(|v| *v > 0).unwrap_or(crate::image_size::DEFAULT_MAX_PIXELS);
        if assert_image_dimensions(&res.buffer, max_side, max_pixels).is_err() {
            return Err(err(SafeFetchCode::BadImage, "That image is too large or is not a supported format."));
        }
        Ok(res.buffer)
    }
}

/// Options of [`safe_fetch_image`].
#[derive(Debug, Clone)]
pub struct ImageOptions {
    pub max_bytes: u64,
    pub timeout_ms: u64,
    pub max_side: Option<u64>,
    pub max_pixels: Option<u64>,
    pub headers: Vec<(String, String)>,
}

impl Default for ImageOptions {
    fn default() -> Self {
        Self { max_bytes: DEFAULT_IMAGE_MAX_BYTES, timeout_ms: 8_000, max_side: None, max_pixels: None, headers: Vec::new() }
    }
}

fn with_default_header(opts: &FetchOptions, name: &str, value: &str) -> FetchOptions {
    let mut o = opts.clone();
    if !o.headers.iter().any(|(k, _)| k.eq_ignore_ascii_case(name)) {
        o.headers.insert(0, (name.into(), value.into()));
    }
    o
}

async fn read_body(res: reqwest::Response, encoding: &str, max_bytes: u64, deadline: Instant, timeout_ms: u64) -> Result<Vec<u8>, SafeFetchError> {
    let too_large = || err(SafeFetchCode::TooLarge, format!("The response is larger than {max_bytes} bytes."));
    let mut decoder = Decoder::new(encoding, max_bytes);
    let mut stream = res.bytes_stream();
    loop {
        let next = match tokio::time::timeout_at(deadline, stream.next()).await {
            Err(_) => return Err(timeout_error(timeout_ms)),
            Ok(n) => n,
        };
        match next {
            None => break,
            Some(Err(e)) => {
                let code = network_code(&e);
                let msg = if code.contains("connection closed before message completed") || code.contains("end of file") {
                    "Network error: connection closed early".to_string()
                } else {
                    format!("Network error: {code}")
                };
                return Err(err(SafeFetchCode::Network, msg));
            }
            Some(Ok(chunk)) => {
                if let Err(_e) = decoder.write(&chunk) {
                    if decoder.over() {
                        return Err(too_large());
                    }
                    let code = if decoder.kind() == "br" { "ERR__ERROR_FORMAT_PADDING_1" } else { "Z_DATA_ERROR" };
                    return Err(err(SafeFetchCode::Network, format!("Network error: {code}")));
                }
            }
        }
    }
    match decoder.finish() {
        Ok(sink) => Ok(sink.data),
        Err((true, _)) => Err(too_large()),
        Err((false, code)) => Err(err(SafeFetchCode::Network, format!("Network error: {code}"))),
    }
}

static DEFAULT_FETCHER: LazyLock<SafeFetcher> = LazyLock::new(SafeFetcher::default);

/// `safeFetch(url, opts)` with the system resolver.
pub async fn safe_fetch(url: &str, opts: &FetchOptions) -> Result<FetchResponse, SafeFetchError> {
    DEFAULT_FETCHER.fetch(url, opts).await
}

/// `safeFetchJson(url, opts)`.
pub async fn safe_fetch_json(url: &str, opts: &FetchOptions) -> Result<serde_json::Value, SafeFetchError> {
    DEFAULT_FETCHER.fetch_json(url, opts).await
}

/// `safeFetchText(url, opts)`.
pub async fn safe_fetch_text(url: &str, opts: &FetchOptions) -> Result<String, SafeFetchError> {
    DEFAULT_FETCHER.fetch_text(url, opts).await
}

/// `safeFetchImage(url, opts)`.
pub async fn safe_fetch_image(url: &str, opts: &ImageOptions) -> Result<Vec<u8>, SafeFetchError> {
    DEFAULT_FETCHER.fetch_image(url, opts).await
}

/// `assertPublicUrl(url)`.
pub async fn assert_public_url(url: &str) -> Result<Url, SafeFetchError> {
    DEFAULT_FETCHER.assert_public_url(url).await
}
