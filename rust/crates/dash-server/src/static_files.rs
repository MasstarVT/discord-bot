//! The static UI files of server.js: the whitelist (request path -> file and content type), the files themselves
//! (embedded from `src/dashboard/public/` at build time, or read from a directory), their ETags and gzip copies, and
//! the HTTP caching helpers (`acceptsGzip`, `etagMatches`).

use std::io::Write;
use std::path::{Path, PathBuf};

use base64::Engine;
use sha2::{Digest, Sha256};

/// The only files served: request path -> (file name, content type). `STATIC_FILES`.
pub const STATIC_FILES: [(&str, &str, &str); 23] = [
    ("/", "landing.html", "text/html; charset=utf-8"),
    ("/dashboard", "index.html", "text/html; charset=utf-8"),
    ("/index.html", "index.html", "text/html; charset=utf-8"),
    ("/landing.js", "landing.js", "text/javascript; charset=utf-8"),
    ("/app.js", "app.js", "text/javascript; charset=utf-8"),
    ("/lib.js", "lib.js", "text/javascript; charset=utf-8"),
    ("/forms.js", "forms.js", "text/javascript; charset=utf-8"),
    ("/collections.js", "collections.js", "text/javascript; charset=utf-8"),
    ("/embed-editor.js", "embed-editor.js", "text/javascript; charset=utf-8"),
    ("/tokens.css", "tokens.css", "text/css; charset=utf-8"),
    ("/styles.css", "styles.css", "text/css; charset=utf-8"),
    ("/landing.css", "landing.css", "text/css; charset=utf-8"),
    ("/site.css", "site.css", "text/css; charset=utf-8"),
    ("/leaflet.css", "leaflet.css", "text/css; charset=utf-8"),
    ("/docs", "docs.html", "text/html; charset=utf-8"),
    ("/changelog", "changelog.html", "text/html; charset=utf-8"),
    ("/demo/rank-card.png", "demo/rank-card.png", "image/png"),
    ("/demo/welcome-card.png", "demo/welcome-card.png", "image/png"),
    ("/forms.css", "forms.css", "text/css; charset=utf-8"),
    ("/collections.css", "collections.css", "text/css; charset=utf-8"),
    ("/favicon.svg", "favicon.svg", "image/svg+xml"),
    ("/privacy", "privacy.html", "text/html; charset=utf-8"),
    ("/terms", "terms.html", "text/html; charset=utf-8"),
];

/// The file and content type served at a request path.
pub fn static_file(path: &str) -> Option<(&'static str, &'static str)> {
    STATIC_FILES.iter().find(|(p, _, _)| *p == path).map(|(_, f, t)| (*f, *t))
}

macro_rules! embedded {
    ($($name:literal),* $(,)?) => {
        /// The files of `src/dashboard/public/`, as they were at build time.
        pub const EMBEDDED: &[(&str, &[u8])] = &[
            $(($name, include_bytes!(concat!("../../../../src/dashboard/public/", $name)))),*
        ];
    };
}

embedded!(
    "landing.html",
    "index.html",
    "landing.js",
    "app.js",
    "lib.js",
    "forms.js",
    "collections.js",
    "embed-editor.js",
    "tokens.css",
    "styles.css",
    "landing.css",
    "site.css",
    "leaflet.css",
    "docs.html",
    "changelog.html",
    "demo/rank-card.png",
    "demo/welcome-card.png",
    "forms.css",
    "collections.css",
    "favicon.svg",
    "privacy.html",
    "terms.html",
);

/// CHANGELOG.md at the repo root, as it was at build time.
pub const EMBEDDED_CHANGELOG: &str = include_str!("../../../../CHANGELOG.md");

/// Where the static files come from: the embedded copies, or a directory (server.js `publicDir`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum PublicDir {
    #[default]
    Embedded,
    Path(PathBuf),
}

/// Why a file could not be loaded: missing (ENOENT, not logged) or another problem (logged).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadError {
    NotFound,
    Other(String),
}

impl PublicDir {
    /// The bytes of a file.
    pub fn read(&self, file: &str) -> Result<Vec<u8>, LoadError> {
        match self {
            Self::Embedded => EMBEDDED.iter().find(|(n, _)| *n == file).map(|(_, b)| b.to_vec()).ok_or(LoadError::NotFound),
            Self::Path(dir) => read_file(&dir.join(file)),
        }
    }
}

fn read_file(path: &Path) -> Result<Vec<u8>, LoadError> {
    std::fs::read(path).map_err(|e| if e.kind() == std::io::ErrorKind::NotFound { LoadError::NotFound } else { LoadError::Other(e.to_string()) })
}

/// One static file in memory: its bytes, a gzip copy when that is smaller, and a strong ETag per encoding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaticEntry {
    pub body: Vec<u8>,
    pub etag: String,
    pub gzip: Option<Vec<u8>>,
    pub gzip_etag: String,
}

fn compressible(content_type: &str) -> bool {
    content_type.starts_with("text/") || content_type.starts_with("image/svg+xml")
}

/// `loadStaticEntry`: hashes the (transformed) bytes and gzips them at level 9 when the type is compressible.
pub fn make_entry(body: Vec<u8>, content_type: &str) -> StaticEntry {
    let hash = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(&body));
    let hash = &hash[..27];
    let mut gzip = None;
    if compressible(content_type) {
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::new(9));
        if enc.write_all(&body).is_ok()
            && let Ok(packed) = enc.finish()
            && packed.len() < body.len()
        {
            gzip = Some(packed);
        }
    }
    StaticEntry { body, etag: format!("\"{hash}\""), gzip, gzip_etag: format!("\"{hash}-gz\"") }
}

/// True when an Accept-Encoding header allows gzip (`gzip;q=0` refuses it). `acceptsGzip`.
pub fn accepts_gzip(header: Option<&str>) -> bool {
    for part in header.unwrap_or("").to_lowercase().split(',') {
        let pieces: Vec<&str> = part.split(';').map(bot_util::js::trim).collect();
        if pieces[0] != "gzip" {
            continue;
        }
        return match pieces[1..].iter().find(|x| x.starts_with("q=")) {
            None => true,
            Some(q) => {
                let n = dash_schema::js::to_number(Some(&serde_json::Value::String(q[2..].to_string())));
                n > 0.0
            }
        };
    }
    false
}

/// If-None-Match against the current ETags: `*`, a list, and weak (W/) tags all count. `etagMatches`.
pub fn etag_matches(header: Option<&str>, etags: &[&str]) -> bool {
    let Some(header) = header else { return false };
    let t = bot_util::js::trim(header);
    if t.is_empty() {
        return false;
    }
    if t == "*" {
        return true;
    }
    header.split(',').any(|tag| {
        let tag = bot_util::js::trim(tag);
        let tag = tag.strip_prefix("W/").unwrap_or(tag);
        etags.contains(&tag)
    })
}
