//! Running the dashboard: listening on the configured host and port with the JS server's timeouts, handing every
//! request its peer address (the proxy-trust rule of `clientAddress` reads it), and closing every connection at
//! shutdown (`server.close()` + `server.closeAllConnections()` in dashboard.js). Also the parked server
//! (`createParkedServer`) for a dashboard that is not configured.

use std::convert::Infallible;
use std::future::Future;
use std::net::SocketAddr;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{ConnectInfo, Request};
use axum::response::Response;
use http::{HeaderName, HeaderValue, StatusCode};
use hyper::body::Incoming;
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::net::TcpListener;
use tokio::task::JoinSet;
use tower::ServiceExt;

use crate::server::SECURITY_HEADERS;

/// Node's `server.headersTimeout` of the dashboard: the request head must arrive within 10 s.
pub const HEADERS_TIMEOUT_MS: u64 = 10_000;
/// The parked server's `headersTimeout`.
pub const PARKED_HEADERS_TIMEOUT_MS: u64 = 5_000;

/// Listens on `host:port` (`server.listen(config.port, config.host)`) and serves `router` until `shutdown`
/// resolves; then stops accepting and closes every open connection at once. Fails when the address cannot be bound
/// (dashboard.js logs it and exits 1).
pub async fn serve(router: Router, host: &str, port: u16, shutdown: impl Future<Output = ()> + Send) -> std::io::Result<()> {
    let listener = TcpListener::bind((host, port)).await?;
    serve_listener(listener, router, HEADERS_TIMEOUT_MS, shutdown).await;
    Ok(())
}

/// Serves `router` on a bound listener (each request carries `ConnectInfo<SocketAddr>` of its peer) until `shutdown`
/// resolves, then closes every connection. `headers_timeout_ms`: how long a connection may take to send a request
/// head (also bounds an idle keep-alive connection).
pub async fn serve_listener(listener: TcpListener, router: Router, headers_timeout_ms: u64, shutdown: impl Future<Output = ()> + Send) {
    let mut conns: JoinSet<()> = JoinSet::new();
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            () = &mut shutdown => break,
            accepted = listener.accept() => {
                let Ok((stream, addr)) = accepted else {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    continue;
                };
                let router = router.clone();
                conns.spawn(serve_connection(stream, addr, router, headers_timeout_ms));
            }
            Some(_) = conns.join_next(), if !conns.is_empty() => {}
        }
    }
    drop(listener);
    conns.abort_all();
    while conns.join_next().await.is_some() {}
}

async fn serve_connection(stream: tokio::net::TcpStream, addr: SocketAddr, router: Router, headers_timeout_ms: u64) {
    let service = hyper::service::service_fn(move |mut req: http::Request<Incoming>| {
        req.extensions_mut().insert(ConnectInfo(addr));
        let router = router.clone();
        async move { Ok::<_, Infallible>(router.oneshot(req.map(Body::new)).await.unwrap_or_else(|e| match e {})) }
    });
    let mut builder = hyper::server::conn::http1::Builder::new();
    builder.timer(TokioTimer::new()).header_read_timeout(Duration::from_millis(headers_timeout_ms)).keep_alive(true);
    let _ = builder.serve_connection(TokioIo::new(stream), service).await;
}

/// A stand-in for a dashboard that is not configured (`createParkedServer`): /healthz answers 200 (so a container
/// stays healthy) and everything else 503 naming the missing or invalid keys (names only).
pub fn create_parked_router(missing: &[String], invalid: &[String]) -> Router {
    let mut parts = Vec::new();
    if !missing.is_empty() {
        parts.push(format!("missing {}", missing.join(", ")));
    }
    if !invalid.is_empty() {
        parts.push(format!("invalid {}", invalid.join(", ")));
    }
    let joined = parts.join("; ");
    let message = format!("dashboard not configured: {}", if joined.is_empty() { "unknown problem" } else { &joined });
    Router::new().fallback(move |req: Request| {
        let message = message.clone();
        async move { parked_answer(&req, &message) }
    })
}

fn parked_answer(req: &Request, message: &str) -> Response {
    // Node's `req.url` is the raw request target: an absolute-form target keeps its scheme and host, so it is not
    // `/healthz` even when its path is.
    let uri = req.uri();
    let target = if uri.scheme().is_some() || uri.authority().is_some() {
        uri.to_string()
    } else {
        uri.path_and_query().map(|p| p.as_str().to_string()).unwrap_or_default()
    };
    let path = target.split('?').next().unwrap_or("");
    let head = req.method() == http::Method::HEAD;
    let (status, content_type, body) = if path == "/healthz" {
        (200, "application/json; charset=utf-8", serde_json::json!({ "ok": true, "parked": true }).to_string())
    } else if path.starts_with("/api/") {
        (503, "application/json; charset=utf-8", serde_json::json!({ "error": "not_configured", "message": message }).to_string())
    } else {
        (503, "text/plain; charset=utf-8", format!("{message}\n"))
    };
    let len = body.len();
    let mut res = Response::new(if head { Body::empty() } else { Body::from(body) });
    *res.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::SERVICE_UNAVAILABLE);
    let headers = res.headers_mut();
    let all = SECURITY_HEADERS
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .chain([
            ("Content-Type".to_string(), content_type.to_string()),
            ("Cache-Control".to_string(), "no-store".to_string()),
            ("Content-Length".to_string(), len.to_string()),
        ]);
    for (k, v) in all {
        if let (Ok(n), Ok(v)) = (HeaderName::from_bytes(k.as_bytes()), HeaderValue::from_str(&v)) {
            headers.insert(n, v);
        }
    }
    res
}
