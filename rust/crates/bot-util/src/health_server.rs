//! Port of `src/utils/healthServer.js`: liveness and readiness endpoints for Docker and Uptime Kuma.
//!
//!   GET /healthz → 200 {"status":"alive"} whenever the runtime runs
//!   GET /readyz  → 200 {"status":"ready",...} when the readiness check says so,
//!                  otherwise 503 {"status":"not-ready","reason":...}
//!
//! Any other method is 405 {"error":"method not allowed"}, any other path 404 {"error":"not found"}; HEAD answers with
//! headers only. Headers must arrive within 5 s, a request must complete within 10 s, and an idle keep-alive
//! connection closes after 5 s, as with the Node server's timeouts. A small HTTP/1.1 server on tokio.

use futures::future::BoxFuture;
use serde_json::{Map, Value};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

const HEADERS_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const KEEP_ALIVE_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_HEADER_BYTES: usize = 16 * 1024;

/// The readiness check's answer: `ready` plus any details (`reason`, …) copied into the body after `status`.
#[derive(Debug, Clone, PartialEq)]
pub struct Readiness {
    pub ready: bool,
    pub details: Map<String, Value>,
}

impl Readiness {
    /// Ready, no details.
    pub fn ready() -> Self {
        Self { ready: true, details: Map::new() }
    }

    /// Not ready, with a reason.
    pub fn not_ready(reason: &str) -> Self {
        let mut details = Map::new();
        details.insert("reason".into(), Value::String(reason.into()));
        Self { ready: false, details }
    }
}

/// The readiness check: Err(message) is reported as `{ reason: 'shard-not-ready', error: message }`.
pub type ReadinessCheck = Arc<dyn Fn() -> BoxFuture<'static, Result<Readiness, String>> + Send + Sync>;

/// A running health server.
pub struct HealthServer {
    pub local_addr: SocketAddr,
    task: JoinHandle<()>,
}

impl HealthServer {
    /// Stops accepting connections.
    pub fn close(self) {
        self.task.abort();
    }
}

/// The status line and JSON body for a request (method, request target).
pub async fn respond(method: &str, target: &str, get_readiness: &ReadinessCheck) -> (u16, Value) {
    let obj = |k: &str, v: &str| {
        let mut m = Map::new();
        m.insert(k.into(), Value::String(v.into()));
        Value::Object(m)
    };
    if method != "GET" && method != "HEAD" {
        return (405, obj("error", "method not allowed"));
    }
    let path = target.split('?').next().unwrap_or("");
    if path == "/healthz" {
        return (200, obj("status", "alive"));
    }
    if path != "/readyz" {
        return (404, obj("error", "not found"));
    }
    let result = match get_readiness().await {
        Ok(r) => r,
        Err(message) => {
            let mut details = Map::new();
            details.insert("reason".into(), Value::String("shard-not-ready".into()));
            details.insert("error".into(), Value::String(message));
            Readiness { ready: false, details }
        }
    };
    let mut body = Map::new();
    body.insert("status".into(), Value::String(if result.ready { "ready" } else { "not-ready" }.into()));
    // `{ status, ...details }`: a detail named status replaces the value but keeps the first position.
    for (k, v) in result.details {
        body.insert(k, v);
    }
    (if result.ready { 200 } else { 503 }, Value::Object(body))
}

fn status_text(code: u16) -> &'static str {
    match code {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        503 => "Service Unavailable",
        _ => "",
    }
}

fn http_date() -> String {
    chrono::Utc::now().format("%a, %d %b %Y %H:%M:%S GMT").to_string()
}

async fn handle_connection(mut sock: TcpStream, get_readiness: ReadinessCheck) {
    let mut buf: Vec<u8> = Vec::new();
    let mut first = true;
    loop {
        // Wait for the head of the next request.
        let wait = if first { HEADERS_TIMEOUT } else { KEEP_ALIVE_TIMEOUT };
        let head_end = match tokio::time::timeout(wait, read_head(&mut sock, &mut buf)).await {
            Ok(Some(n)) => n,
            Ok(None) => return,
            Err(_) => {
                if !buf.is_empty() {
                    let _ = sock.write_all(b"HTTP/1.1 408 Request Timeout\r\nConnection: close\r\n\r\n").await;
                }
                return;
            }
        };
        first = false;
        let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
        buf.drain(..head_end + 4);
        let mut lines = head.split("\r\n");
        let request_line = lines.next().unwrap_or("");
        let mut parts = request_line.split(' ');
        let (Some(method), Some(target), Some(version)) = (parts.next(), parts.next(), parts.next()) else {
            let _ = sock.write_all(b"HTTP/1.1 400 Bad Request\r\nConnection: close\r\n\r\n").await;
            return;
        };
        if !version.starts_with("HTTP/1.") || method.is_empty() || !method.bytes().all(|b| b.is_ascii_uppercase()) {
            let _ = sock.write_all(b"HTTP/1.1 400 Bad Request\r\nConnection: close\r\n\r\n").await;
            return;
        }
        let mut content_length = 0usize;
        let mut close = version == "HTTP/1.0";
        for line in lines {
            let Some((k, v)) = line.split_once(':') else { continue };
            let k = k.trim().to_ascii_lowercase();
            let v = v.trim();
            if k == "content-length" {
                content_length = v.parse().unwrap_or(0);
            } else if k == "connection" {
                let v = v.to_ascii_lowercase();
                if v == "close" {
                    close = true;
                } else if v == "keep-alive" {
                    close = false;
                }
            }
        }
        // Discard a request body.
        let body_read = tokio::time::timeout(REQUEST_TIMEOUT, async {
            while buf.len() < content_length {
                let mut chunk = [0u8; 4096];
                match sock.read(&mut chunk).await {
                    Ok(0) | Err(_) => return false,
                    Ok(n) => buf.extend_from_slice(&chunk[..n]),
                }
            }
            true
        })
        .await;
        if !matches!(body_read, Ok(true)) {
            return;
        }
        buf.drain(..content_length.min(buf.len()));

        let (status, body) = match tokio::time::timeout(REQUEST_TIMEOUT, respond(method, target, &get_readiness)).await {
            Ok(r) => r,
            Err(_) => return,
        };
        let body = serde_json::to_string(&body).unwrap_or_default();
        let head_only = method == "HEAD";
        let mut out = format!(
            "HTTP/1.1 {status} {}\r\nContent-Type: application/json\r\nCache-Control: no-store\r\nDate: {}\r\nConnection: {}\r\n",
            status_text(status),
            http_date(),
            if close { "close" } else { "keep-alive" }
        );
        if !close {
            out.push_str("Keep-Alive: timeout=5\r\n");
        }
        out.push_str(&format!("Content-Length: {}\r\n\r\n", if head_only { 0 } else { body.len() }));
        if !head_only {
            out.push_str(&body);
        }
        if sock.write_all(out.as_bytes()).await.is_err() || close {
            return;
        }
    }
}

// Reads until the end of a request head; Some(index of "\r\n\r\n") or None when the connection ends.
async fn read_head(sock: &mut TcpStream, buf: &mut Vec<u8>) -> Option<usize> {
    loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            return Some(i);
        }
        if buf.len() > MAX_HEADER_BYTES {
            let _ = sock.write_all(b"HTTP/1.1 431 Request Header Fields Too Large\r\nConnection: close\r\n\r\n").await;
            return None;
        }
        let mut chunk = [0u8; 4096];
        match sock.read(&mut chunk).await {
            Ok(0) | Err(_) => return None,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
}

/// Starts the server on `port` (all interfaces). A bind error (an invalid or busy HEALTH_PORT) goes to `on_error`
/// and returns None, as the JS reports it through the server's 'error' event.
pub async fn start_health_server(
    port: u16,
    get_readiness: ReadinessCheck,
    on_error: impl Fn(std::io::Error) + Send + Sync + 'static,
) -> Option<HealthServer> {
    // Node listens on '::' with dual-stack. Bind IPv6 first, then IPv4 on the same port: on systems where the IPv6
    // socket already takes IPv4 (Linux) the second bind is refused and not needed; elsewhere (Windows) both serve.
    let mut listeners = Vec::new();
    let mut first_err = None;
    match TcpListener::bind(("::", port)).await {
        Ok(l) => listeners.push(l),
        Err(e) => first_err = Some(e),
    }
    let v4_port = listeners.first().and_then(|l| l.local_addr().ok()).map_or(port, |a| a.port());
    match TcpListener::bind(("0.0.0.0", v4_port)).await {
        Ok(l) => listeners.push(l),
        Err(e) => {
            if listeners.is_empty() {
                on_error(first_err.unwrap_or(e));
                return None;
            }
        }
    }
    let local_addr = match listeners.last().map(TcpListener::local_addr) {
        Some(Ok(a)) => a,
        Some(Err(e)) => {
            on_error(e);
            return None;
        }
        None => return None,
    };
    let on_error = Arc::new(on_error);
    let task = tokio::spawn(async move {
        let mut accepts = Vec::new();
        for listener in listeners {
            let check = get_readiness.clone();
            let on_error = on_error.clone();
            accepts.push(tokio::spawn(async move {
                loop {
                    match listener.accept().await {
                        Ok((sock, _)) => {
                            tokio::spawn(handle_connection(sock, check.clone()));
                        }
                        Err(e) => on_error(e),
                    }
                }
            }));
        }
        let _guard = AbortOnDrop(accepts);
        std::future::pending::<()>().await;
    });
    Some(HealthServer { local_addr, task })
}

struct AbortOnDrop(Vec<JoinHandle<()>>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        for h in &self.0 {
            h.abort();
        }
    }
}
