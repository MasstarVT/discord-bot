//! Port of `import 'dotenv/config'` (dotenv 16.6.1, the version in package-lock.json): the first line of index.js.
//!
//! * The file is `.env` in the current directory, or `DOTENV_CONFIG_PATH` / the `dotenv_config_path=...` argument
//!   (`~` is the home directory). A missing file is not an error.
//! * The parser is dotenv's own `LINE` regex, run with ECMAScript semantics (regress): `KEY=value`, `export KEY=`,
//!   `KEY: value`, single / double / backtick quotes, `#` comments, multi-line quoted values; `\n` and `\r` are
//!   expanded inside double quotes only. There is no `$VAR` expansion (dotenvy would expand it).
//! * A variable that is already set wins, unless `DOTENV_CONFIG_OVERRIDE` / `dotenv_config_override=` is set to any
//!   non-empty value (`Boolean(options.override)`).

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use indexmap::IndexMap;

/// dotenv's `LINE`, byte for byte.
const LINE: &str = r#"(?:^|^)\s*(?:export\s+)?([\w.-]+)(?:\s*=\s*?|:\s+?)(\s*'(?:\\'|[^'])*'|\s*"(?:\\"|[^"])*"|\s*`(?:\\`|[^`])*`|[^#\r\n]+)?\s*(?:#.*)?(?:$|$)"#;

static LINE_RE: LazyLock<regress::Regex> = LazyLock::new(|| regress::Regex::with_flags(LINE, "mg").expect("dotenv LINE compiles"));
static QUOTES_RE: LazyLock<regress::Regex> =
    LazyLock::new(|| regress::Regex::with_flags(r#"^(['"`])([\s\S]*)\1$"#, "mg").expect("dotenv quote regex compiles"));
static CRLF_RE: LazyLock<regress::Regex> = LazyLock::new(|| regress::Regex::with_flags(r"\r\n?", "mg").expect("crlf regex compiles"));

/// `String.prototype.replace` with a global regex and a replacement built from each match.
fn replace_all(re: &regress::Regex, text: &str, f: impl Fn(&regress::Match) -> String) -> String {
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    for m in re.find_iter(text) {
        out.push_str(&text[last..m.start()]);
        out.push_str(&f(&m));
        last = m.end();
    }
    out.push_str(&text[last..]);
    out
}

/// `dotenv.parse(src)`: the keys in file order (a repeated key keeps its first position and its last value, as a JS
/// object does).
pub fn parse(src: &str) -> IndexMap<String, String> {
    let mut obj = IndexMap::new();
    let lines = replace_all(&CRLF_RE, src, |_| "\n".to_string());
    for m in LINE_RE.find_iter(&lines) {
        let Some(key) = m.group(1).map(|r| lines[r].to_string()) else { continue };
        let raw = m.group(2).map(|r| &lines[r]).unwrap_or("");
        let mut value = bot_util::js::trim(raw).to_string();
        let maybe_quote = value.chars().next();
        value = replace_all(&QUOTES_RE, &value, |q| q.group(2).map(|r| value[r].to_string()).unwrap_or_default());
        if maybe_quote == Some('"') {
            value = value.replace("\\n", "\n").replace("\\r", "\r");
        }
        obj.insert(key, value);
    }
    obj
}

/// The `config()` options dotenv/config reads from `DOTENV_CONFIG_*` and from `dotenv_config_*=` arguments.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Options {
    /// `path` (one file); `None` is `<cwd>/.env`.
    pub path: Option<String>,
    /// `Boolean(override)`.
    pub override_existing: bool,
}

/// `Object.assign({}, env-options, cli-options(process.argv))`: an argument wins over the environment variable.
pub fn options(env: impl Fn(&str) -> Option<String>, argv: &[String]) -> Options {
    let mut path = env("DOTENV_CONFIG_PATH");
    let mut over = env("DOTENV_CONFIG_OVERRIDE");
    for arg in argv {
        if let Some(rest) = arg.strip_prefix("dotenv_config_path=").filter(|r| !r.is_empty()) {
            path = Some(rest.to_string());
        } else if let Some(rest) = arg.strip_prefix("dotenv_config_override=").filter(|r| !r.is_empty()) {
            over = Some(rest.to_string());
        }
    }
    Options { path, override_existing: over.is_some_and(|v| !v.is_empty()) }
}

/// `_resolveHome(path)`.
fn resolve_home(p: &str) -> PathBuf {
    match p.strip_prefix('~') {
        Some(rest) => {
            let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from).unwrap_or_default();
            home.join(rest.trim_start_matches(['/', '\\']))
        }
        None => PathBuf::from(p),
    }
}

/// The file dotenv reads.
pub fn file_path(cwd: &Path, opts: &Options) -> PathBuf {
    match &opts.path {
        Some(p) => {
            let p = resolve_home(p);
            if p.is_absolute() { p } else { cwd.join(p) }
        }
        None => cwd.join(".env"),
    }
}

/// What `config()` sets: the pairs from the file that `populate` would write, given which variables already exist.
/// The error is the read failure (a missing `.env` included), which dotenv/config ignores.
pub fn plan(cwd: &Path, opts: &Options, is_set: impl Fn(&str) -> bool) -> (Vec<(String, String)>, Option<std::io::Error>) {
    let file = file_path(cwd, opts);
    let text = match std::fs::read(&file) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(e) => return (Vec::new(), Some(e)),
    };
    let set = parse(&text)
        .into_iter()
        .filter(|(k, _)| opts.override_existing || !is_set(k))
        // A key the OS cannot hold (empty, or with '=' / NUL) is left out instead of aborting the start.
        .filter(|(k, v)| !k.is_empty() && !k.contains('=') && !k.contains('\0') && !v.contains('\0'))
        .collect();
    (set, None)
}

/// `import 'dotenv/config'`. Call before any other thread exists (`std::env::set_var` is not thread-safe).
pub fn load_into_process_env() {
    let argv: Vec<String> = std::env::args().collect();
    let opts = options(|k| std::env::var(k).ok(), &argv);
    let cwd = std::env::current_dir().unwrap_or_default();
    let (pairs, _err) = plan(&cwd, &opts, |k| std::env::var_os(k).is_some());
    for (k, v) in pairs {
        // SAFETY: called first thing in `main`, before the tokio runtime or any other thread is started.
        unsafe { std::env::set_var(k, v) };
    }
}
