//! Ports of the safeFetch tests in test/utils.test.js. The JS stubs `dns.promises.lookup` and `http.request`; here a
//! test resolver answers the DNS step and a dial map sends the validated public address to a local server, so every
//! URL and address check still runs on the address the JS test uses.

use bot_util::safe_fetch::{self, Body, FetchOptions, ImageOptions, Resolver, SafeFetchCode, SafeFetchError, SafeFetcher};
use futures::future::BoxFuture;
use std::collections::HashMap;
use std::io::Write;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

struct FakeDns(HashMap<String, Vec<IpAddr>>);

impl Resolver for FakeDns {
    fn lookup(&self, host: &str) -> BoxFuture<'static, std::io::Result<Vec<IpAddr>>> {
        let r = self.0.get(host).cloned().ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "ENOTFOUND"));
        Box::pin(async move { r })
    }
}

#[derive(Debug, Clone)]
struct Seen {
    method: String,
    path: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

enum Reply {
    Raw(Vec<u8>),
    /// Headers then part of the body, then silence.
    Stall(Vec<u8>),
    Hang,
}

fn response(status: u16, headers: &[(&str, &str)], body: &[u8]) -> Reply {
    let mut out = format!("HTTP/1.1 {status} Status\r\n");
    let has_len = headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("content-length"));
    for (k, v) in headers {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    if !has_len {
        out.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    out.push_str("Connection: close\r\n\r\n");
    let mut bytes = out.into_bytes();
    bytes.extend_from_slice(body);
    Reply::Raw(bytes)
}

type Handler = Arc<dyn Fn(&Seen, usize) -> Reply + Send + Sync>;

/// A local HTTP server; returns its port and the requests it saw.
async fn server(handler: Handler) -> (u16, Arc<Mutex<Vec<Seen>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let s2 = seen.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else { return };
            let handler = handler.clone();
            let seen = s2.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let head_end = loop {
                    let mut chunk = [0u8; 4096];
                    let Ok(n) = sock.read(&mut chunk).await else { return };
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break i;
                    }
                };
                let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
                let mut lines = head.split("\r\n");
                let mut rl = lines.next().unwrap_or("").split(' ');
                let method = rl.next().unwrap_or("").to_string();
                let path = rl.next().unwrap_or("").to_string();
                let headers: HashMap<String, String> =
                    lines.filter_map(|l| l.split_once(':')).map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string())).collect();
                let len: usize = headers.get("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
                let mut body = buf[head_end + 4..].to_vec();
                while body.len() < len {
                    let mut chunk = [0u8; 4096];
                    match sock.read(&mut chunk).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => body.extend_from_slice(&chunk[..n]),
                    }
                }
                let req = Seen { method, path, headers, body };
                let n = {
                    let mut g = seen.lock().unwrap();
                    g.push(req.clone());
                    g.len()
                };
                match handler(&req, n) {
                    Reply::Raw(bytes) => {
                        let _ = sock.write_all(&bytes).await;
                        let _ = sock.shutdown().await;
                    }
                    Reply::Stall(bytes) => {
                        let _ = sock.write_all(&bytes).await;
                        tokio::time::sleep(Duration::from_secs(30)).await;
                    }
                    Reply::Hang => tokio::time::sleep(Duration::from_secs(30)).await,
                }
            });
        }
    });
    (port, seen)
}

const PUBLIC: &str = "93.184.216.34";

fn fetcher(hosts: &[(&str, &[&str])]) -> SafeFetcher {
    let map = hosts.iter().map(|(h, a)| (h.to_string(), a.iter().map(|x| x.parse().unwrap()).collect())).collect();
    SafeFetcher::with_resolver(Arc::new(FakeDns(map))).with_dial_map(Arc::new(|_| "127.0.0.1".parse().unwrap()))
}

fn code(r: Result<impl std::fmt::Debug, SafeFetchError>) -> SafeFetchCode {
    r.expect_err("expected an error").code
}

#[test]
fn safe_fetch_error_carries_code_status_headers_and_retry_after() {
    let e = SafeFetchError::with_status(SafeFetchCode::HttpError, 404);
    assert_eq!(e.status, Some(404));
    assert_eq!(e.code.as_str(), "HTTP_ERROR");
    assert_eq!(e.message, "HTTP 404");
    assert_eq!(e.name(), "SafeFetchError");
    assert_eq!(SafeFetchError::new(SafeFetchCode::Timeout, None).message, "TIMEOUT");
    let ua = safe_fetch::default_user_agent();
    assert!(ua.starts_with("DiscordBot/1.0 (+self-hosted; contact: ") && ua.ends_with(')'), "{ua}");
}

#[tokio::test]
async fn assert_public_url_rejects_bad_urls_before_any_dns_lookup() {
    let f = fetcher(&[]);
    for (url, want) in [
        ("ftp://x", SafeFetchCode::InvalidUrl),
        ("not a url", SafeFetchCode::InvalidUrl),
        ("http://127.0.0.1/", SafeFetchCode::BlockedAddress),
        ("http://2130706433/", SafeFetchCode::BlockedAddress),
        ("http://0x7f.1/", SafeFetchCode::BlockedAddress),
        ("http://[::1]/", SafeFetchCode::BlockedAddress),
        ("http://169.254.169.254/latest/meta-data", SafeFetchCode::BlockedAddress),
        ("http://user:pw@example.com", SafeFetchCode::InvalidUrl),
        ("http://:pw@example.com", SafeFetchCode::InvalidUrl),
        ("http://missing.test/", SafeFetchCode::DnsFailed),
    ] {
        assert_eq!(code(f.assert_public_url(url).await), want, "{url}");
    }
    assert_eq!(f.assert_public_url("http://1.1.1.1/a?b").await.unwrap().as_str(), "http://1.1.1.1/a?b");
}

#[tokio::test]
async fn pins_the_validated_addresses_and_sends_the_js_headers() {
    let (port, seen) = server(Arc::new(|_, _| response(200, &[("content-type", "application/json")], b"{\"a\":1}"))).await;
    let f = fetcher(&[("api.test", &[PUBLIC, "2606:4700::1"])]);
    let data = f.fetch_json(&format!("http://api.test:{port}/x?y=1"), &FetchOptions::default()).await.unwrap();
    assert_eq!(data, serde_json::json!({"a": 1}));
    let req = seen.lock().unwrap()[0].clone();
    assert_eq!(req.path, "/x?y=1");
    assert_eq!(req.headers["host"], format!("api.test:{port}"));
    assert_eq!(req.headers["user-agent"], safe_fetch::default_user_agent());
    assert_eq!(req.headers["accept"], "application/json");
    assert_eq!(req.headers["accept-encoding"], "gzip, deflate, br");
}

#[tokio::test]
async fn refuses_a_hostname_that_resolves_to_a_private_address_without_connecting() {
    let (port, seen) = server(Arc::new(|_, _| response(200, &[], b""))).await;
    let f = fetcher(&[("evil.test", &["10.0.0.5"]), ("mixed.test", &[PUBLIC, "169.254.169.254"])]);
    for host in ["evil.test", "mixed.test"] {
        assert_eq!(code(f.fetch(&format!("http://{host}:{port}/"), &FetchOptions::default()).await), SafeFetchCode::BlockedAddress);
    }
    assert_eq!(code(f.fetch(&format!("http://missing.test:{port}/"), &FetchOptions::default()).await), SafeFetchCode::DnsFailed);
    assert_eq!(seen.lock().unwrap().len(), 0);
}

#[tokio::test]
async fn re_checks_every_redirect_hop() {
    let hops: HashMap<&str, &str> = [
        ("a.test", "http://127.0.0.1/admin"),
        ("b.test", "http://internal.test/x"),
        ("c.test", "http://[::1]:8080/"),
        ("d.test", "ftp://other.test/file"),
        ("e.test", "http://169.254.169.254/latest/meta-data/"),
    ]
    .into_iter()
    .collect();
    let hops2 = hops.clone();
    let (port, seen) = server(Arc::new(move |req, _| {
        let host = req.headers["host"].split(':').next().unwrap_or("").to_string();
        response(302, &[("location", hops2[host.as_str()])], b"")
    }))
    .await;
    let f = fetcher(&[("a.test", &[PUBLIC]), ("b.test", &[PUBLIC]), ("c.test", &[PUBLIC]), ("d.test", &[PUBLIC]), ("e.test", &[PUBLIC]), ("internal.test", &["192.168.1.10"])]);
    for (host, want) in [
        ("a.test", SafeFetchCode::BlockedAddress),
        ("b.test", SafeFetchCode::BlockedAddress),
        ("c.test", SafeFetchCode::BlockedAddress),
        ("d.test", SafeFetchCode::InvalidUrl),
        ("e.test", SafeFetchCode::BlockedAddress),
    ] {
        assert_eq!(code(f.fetch(&format!("http://{host}:{port}/"), &FetchOptions::default()).await), want, "{host}");
    }
    assert_eq!(seen.lock().unwrap().len(), 5, "only the first hop of each chain was requested");
}

#[tokio::test]
async fn follows_public_redirects_resets_the_method_and_drops_credentials_across_origins() {
    let port_cell = Arc::new(Mutex::new(0u16));
    let pc = port_cell.clone();
    let (port, seen) = server(Arc::new(move |_, n| {
        if n == 1 {
            let p = *pc.lock().unwrap();
            response(302, &[("location", &format!("http://b.test:{p}/final?x=1"))], b"")
        } else {
            response(200, &[("content-type", "text/plain; charset=utf-8")], b"done")
        }
    }))
    .await;
    *port_cell.lock().unwrap() = port;
    let f = fetcher(&[("a.test", &[PUBLIC]), ("b.test", &["93.184.216.35"])]);
    let opts = FetchOptions {
        method: "POST".into(),
        body: Some(Body::Json(serde_json::json!({"hello": "world"}))),
        headers: vec![("Authorization".into(), "Bearer secret".into()), ("Cookie".into(), "a=b".into()), ("Host".into(), "ignored".into())],
        ..FetchOptions::default()
    };
    let res = f.fetch(&format!("http://a.test:{port}/start"), &opts).await.unwrap();
    assert_eq!(res.text(), "done");
    assert_eq!(res.url, format!("http://b.test:{port}/final?x=1"));
    assert!(res.ok);
    let reqs = seen.lock().unwrap().clone();
    assert_eq!(reqs.len(), 2);
    assert_eq!(reqs[0].method, "POST");
    assert_eq!(reqs[0].headers["content-type"], "application/json");
    assert_eq!(reqs[0].body, b"{\"hello\":\"world\"}");
    assert_eq!(reqs[0].headers["authorization"], "Bearer secret");
    assert_eq!(reqs[0].headers["host"], format!("a.test:{port}"), "the caller's Host header is dropped");
    assert_eq!(reqs[1].method, "GET");
    assert_eq!(reqs[1].path, "/final?x=1");
    assert!(!reqs[1].headers.contains_key("authorization"));
    assert!(!reqs[1].headers.contains_key("cookie"));
    assert!(!reqs[1].headers.contains_key("content-type"));
    assert!(!reqs[1].headers.contains_key("content-length"));
}

#[tokio::test]
async fn throws_too_many_redirects() {
    let (port, _) = server(Arc::new(|_, _| response(301, &[("location", "/again")], b""))).await;
    let f = fetcher(&[("loop.test", &[PUBLIC])]);
    let opts = FetchOptions { max_redirects: 2, ..FetchOptions::default() };
    assert_eq!(code(f.fetch(&format!("http://loop.test:{port}/"), &opts).await), SafeFetchCode::TooManyRedirects);
}

fn gzip(data: &[u8]) -> Vec<u8> {
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    e.write_all(data).unwrap();
    e.finish().unwrap()
}

fn zlib(data: &[u8]) -> Vec<u8> {
    let mut e = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    e.write_all(data).unwrap();
    e.finish().unwrap()
}

#[tokio::test]
async fn caps_the_response_size_including_after_decompression() {
    let bomb = gzip(&vec![0u8; 5_000_000]);
    let hello = gzip(b"hello gzip");
    let deflated = zlib(b"hello deflate");
    let (port, _) = server(Arc::new(move |req, _| match req.path.as_str() {
        "/declared" => {
            let mut r = b"HTTP/1.1 200 OK\r\nContent-Length: 5000\r\nConnection: close\r\n\r\nx".to_vec();
            r.extend_from_slice(b"");
            Reply::Raw(r)
        }
        "/stream" => {
            let mut r = b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n".to_vec();
            r.extend(std::iter::repeat_n(b'x', 5000));
            Reply::Raw(r)
        }
        "/bomb" => response(200, &[("content-encoding", "gzip")], &bomb),
        "/gzip" => response(200, &[("content-encoding", "gzip"), ("content-type", "text/plain")], &hello),
        "/deflate" => response(200, &[("content-encoding", "deflate")], &deflated),
        "/corrupt" => response(200, &[("content-encoding", "gzip")], b"not gzip at all"),
        _ => response(200, &[], b"small"),
    }))
    .await;
    let f = fetcher(&[("big.test", &[PUBLIC])]);
    let opts = FetchOptions { max_bytes: 1000, ..FetchOptions::default() };
    for path in ["/declared", "/stream", "/bomb"] {
        assert_eq!(code(f.fetch(&format!("http://big.test:{port}{path}"), &opts).await), SafeFetchCode::TooLarge, "{path}");
    }
    let res = f.fetch(&format!("http://big.test:{port}/gzip"), &opts).await.unwrap();
    assert_eq!(res.text(), "hello gzip");
    assert_eq!(res.headers["content-encoding"], "gzip", "headers are kept as sent");
    assert_eq!(f.fetch(&format!("http://big.test:{port}/deflate"), &opts).await.unwrap().text(), "hello deflate");
    assert_eq!(f.fetch(&format!("http://big.test:{port}/ok"), &opts).await.unwrap().text(), "small");
    let e = f.fetch(&format!("http://big.test:{port}/corrupt"), &opts).await.unwrap_err();
    assert_eq!(e.code, SafeFetchCode::Network);
    assert!(e.message.starts_with("Network error: Z_"), "{}", e.message);
}

#[tokio::test]
async fn times_out_when_the_server_never_answers_or_stalls() {
    let (port, _) = server(Arc::new(|req, _| {
        if req.path == "/stall" {
            Reply::Stall(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\npartial".to_vec())
        } else {
            Reply::Hang
        }
    }))
    .await;
    let f = fetcher(&[("slow.test", &[PUBLIC])]);
    let e = f.fetch(&format!("http://slow.test:{port}/"), &FetchOptions { timeout_ms: 50, ..FetchOptions::default() }).await.unwrap_err();
    assert_eq!(e.code, SafeFetchCode::Timeout);
    assert_eq!(e.message, "Request timed out after 50 ms.");
    let e = f.fetch(&format!("http://slow.test:{port}/stall"), &FetchOptions { timeout_ms: 80, ..FetchOptions::default() }).await.unwrap_err();
    assert_eq!(e.code, SafeFetchCode::Timeout);
}

#[tokio::test]
async fn maps_socket_errors_to_network() {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    drop(l);
    let f = fetcher(&[("down.test", &[PUBLIC])]);
    let e = f.fetch(&format!("http://down.test:{port}/"), &FetchOptions::default()).await.unwrap_err();
    assert_eq!(e.code, SafeFetchCode::Network);
    assert!(e.message.contains("ECONNREFUSED"), "{}", e.message);
}

fn png(w: u32, h: u32) -> Vec<u8> {
    let mut b = vec![0u8; 24];
    b[..8].copy_from_slice(&[0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]);
    b[8..12].copy_from_slice(&13u32.to_be_bytes());
    b[12..16].copy_from_slice(b"IHDR");
    b[16..20].copy_from_slice(&w.to_be_bytes());
    b[20..24].copy_from_slice(&h.to_be_bytes());
    b
}

#[tokio::test]
async fn wrappers_turn_http_failures_into_errors_with_status_headers_and_retry_after() {
    let (port, _) = server(Arc::new(|req, _| match req.path.as_str() {
        "/limited" => response(429, &[("retry-after", "3"), ("x-ratelimit-remaining", "0")], b"{}"),
        "/unavailable" => response(503, &[("retry-after", "Wed, 21 Oct 2015 07:28:00 GMT")], b""),
        "/notfound" => response(404, &[], b"no"),
        "/badjson" => response(200, &[], b"<html>"),
        "/image" => response(200, &[("content-type", "image/png")], b"not really a png"),
        "/svg" => response(200, &[("content-type", "image/svg+xml")], b"<svg/>"),
        "/text" => response(200, &[("content-type", "text/html")], b"hi"),
        "/latin1" => response(200, &[("content-type", "text/plain; charset=ISO-8859-1")], &[0x63, 0x61, 0x66, 0xe9]),
        "/notmodified" => Reply::Raw(b"HTTP/1.1 304 Not Modified\r\nConnection: close\r\n\r\n".to_vec()),
        "/bomb" => response(200, &[("content-type", "image/png")], &png(30000, 30000)),
        _ => response(200, &[("content-type", "image/png")], &png(64, 64)),
    }))
    .await;
    let f = fetcher(&[("api.test", &[PUBLIC])]);
    let u = |p: &str| format!("http://api.test:{port}{p}");
    let d = FetchOptions::default();

    let e = f.fetch_json(&u("/limited"), &d).await.unwrap_err();
    assert_eq!((e.code, e.status, e.retry_after_ms), (SafeFetchCode::HttpError, Some(429), Some(3000.0)));
    assert_eq!(e.headers.as_ref().unwrap()["x-ratelimit-remaining"], "0");
    let e = f.fetch_text(&u("/unavailable"), &d).await.unwrap_err();
    assert_eq!((e.status, e.retry_after_ms), (Some(503), Some(0.0)), "a past date is 0 ms");
    let e = f.fetch_text(&u("/notfound"), &d).await.unwrap_err();
    assert_eq!((e.code, e.status, e.retry_after_ms), (SafeFetchCode::HttpError, Some(404), None));
    let e = f.fetch_json(&u("/badjson"), &d).await.unwrap_err();
    assert_eq!(e.code, SafeFetchCode::HttpError);
    assert!(e.message.contains("not valid JSON"));
    assert_eq!(f.fetch_text(&u("/text"), &d).await.unwrap(), "hi");
    assert_eq!(f.fetch_text(&u("/latin1"), &d).await.unwrap(), "café", "the charset decides the decoding");

    let conditional = f.fetch(&u("/notmodified"), &FetchOptions { headers: vec![("If-None-Match".into(), "abc".into())], ..d.clone() }).await.unwrap();
    assert_eq!(conditional.status, 304);
    assert!(!conditional.ok);
    assert_eq!(conditional.status_text, "Not Modified");

    let io = ImageOptions::default();
    assert_eq!(code(f.fetch_image(&u("/svg"), &io).await), SafeFetchCode::BadContentType);
    assert_eq!(code(f.fetch_image(&u("/text"), &io).await), SafeFetchCode::BadContentType);
    assert_eq!(code(f.fetch_image(&u("/image"), &io).await), SafeFetchCode::BadImage);
    assert_eq!(code(f.fetch_image(&u("/bomb"), &io).await), SafeFetchCode::BadImage);
    let png = f.fetch_image(&u("/small"), &io).await.unwrap();
    assert_eq!(bot_util::image_size::read_image_size(&png).unwrap().width, 64);
}

#[tokio::test]
async fn never_connects_to_a_loopback_server_by_address_or_by_name() {
    let hits = Arc::new(Mutex::new(0));
    let h2 = hits.clone();
    let (port, _) = server(Arc::new(move |_, _| {
        *h2.lock().unwrap() += 1;
        response(200, &[], b"secret")
    }))
    .await;
    // The real resolver: 'localhost' resolves to loopback and is refused.
    for url in [format!("http://127.0.0.1:{port}/"), format!("http://localhost:{port}/"), format!("http://[::1]:{port}/")] {
        let e = safe_fetch::safe_fetch(&url, &FetchOptions::default()).await.unwrap_err();
        assert_eq!(e.code, SafeFetchCode::BlockedAddress, "{url}");
        assert_eq!(e.message, "That address points to a private or reserved network.");
    }
    assert_eq!(*hits.lock().unwrap(), 0);
}

#[test]
fn decode_text_follows_text_decoder() {
    assert_eq!(safe_fetch::decode_text(b"\xef\xbb\xbfhi", None), "hi", "a UTF-8 BOM is dropped");
    assert_eq!(safe_fetch::decode_text(&[0xe9], Some("text/plain; charset=\"windows-1252\"")), "é");
    assert_eq!(safe_fetch::decode_text(&[0xe9], Some("text/plain; charset=nonsense")), "\u{fffd}", "unknown labels are UTF-8");
    assert_eq!(safe_fetch::decode_text(b"x", Some("text/plain; charset=replacement")), "x", "TextDecoder refuses 'replacement'");
    assert_eq!(safe_fetch::parse_retry_after(Some(" 12 ")), Some(12_000.0));
    assert_eq!(safe_fetch::parse_retry_after(Some("soon")), None);
}
