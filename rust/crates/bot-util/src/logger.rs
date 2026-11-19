//! Port of `src/utils/logger.js`, the workspace logger (the JS rule "logger, never console" becomes "no println! or
//! eprintln! outside this module"). Same levels, same line format
//! `[YYYY-MM-DD HH:MM:SS] [LEVEL] [Shard N] message`, local time, chalk's colours when stdout is a terminal (or
//! FORCE_COLOR is set), debug lines dropped when NODE_ENV=production, info/success/debug on stdout and warn/error on
//! stderr. Strings have C0 controls and DEL escaped so a value can never forge a line; objects are printed the way
//! Node's `util.inspect(value, { depth: 3, breakLength: Infinity })` prints them.

use crate::js;
use serde_json::Value;
use std::io::{IsTerminal, Write};
use std::sync::{Arc, LazyLock, Mutex, RwLock};

/// One argument of a log call (the JS logger takes any values).
#[derive(Debug, Clone)]
pub enum LogArg {
    /// A string: printed as-is, controls escaped.
    Str(String),
    /// A number: printed as JS `String(n)`.
    Num(f64),
    Bool(bool),
    Null,
    Undefined,
    /// An object or array: printed with util.inspect rules.
    Json(Value),
    /// An Error: its message inline, its stack (when given) on its own lines after an error line.
    Error { message: String, stack: Option<String> },
}

impl From<&str> for LogArg {
    fn from(s: &str) -> Self {
        Self::Str(s.to_string())
    }
}
impl From<String> for LogArg {
    fn from(s: String) -> Self {
        Self::Str(s)
    }
}
impl From<&String> for LogArg {
    fn from(s: &String) -> Self {
        Self::Str(s.clone())
    }
}
impl From<f64> for LogArg {
    fn from(n: f64) -> Self {
        Self::Num(n)
    }
}
macro_rules! from_int {
    ($($t:ty),*) => {$(impl From<$t> for LogArg { fn from(n: $t) -> Self { Self::Num(n as f64) } })*};
}
from_int!(i8, i16, i32, i64, u8, u16, u32, u64, usize, isize);
impl From<bool> for LogArg {
    fn from(b: bool) -> Self {
        Self::Bool(b)
    }
}
impl From<Value> for LogArg {
    fn from(v: Value) -> Self {
        match v {
            Value::Null => Self::Null,
            Value::Bool(b) => Self::Bool(b),
            Value::Number(n) => Self::Num(n.as_f64().unwrap_or(f64::NAN)),
            Value::String(s) => Self::Str(s),
            other => Self::Json(other),
        }
    }
}
impl<T: Into<LogArg>> From<Option<T>> for LogArg {
    fn from(v: Option<T>) -> Self {
        v.map_or(Self::Null, Into::into)
    }
}

impl LogArg {
    /// An error: the message inline and its source chain as the "stack".
    pub fn error(err: &(dyn std::error::Error + 'static)) -> Self {
        let mut stack = format!("Error: {err}");
        let mut src = err.source();
        while let Some(s) = src {
            stack.push_str(&format!("\n    caused by: {s}"));
            src = s.source();
        }
        Self::Error { message: err.to_string(), stack: Some(stack) }
    }
}

/// A log level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Warn,
    Error,
    Debug,
    Success,
}

impl Level {
    fn label(self) -> &'static str {
        match self {
            Self::Info => "INFO ",
            Self::Warn => "WARN ",
            Self::Error => "ERROR",
            Self::Debug => "DEBUG",
            Self::Success => "OK   ",
        }
    }
    fn color(self) -> &'static str {
        match self {
            Self::Info => "36",
            Self::Warn => "33",
            Self::Error => "31",
            Self::Debug => "35",
            Self::Success => "32",
        }
    }
    fn to_stderr(self) -> bool {
        matches!(self, Self::Warn | Self::Error)
    }
}

type Sink = Box<dyn Fn(Level, &str) + Send + Sync>;

static SINK: RwLock<Option<Sink>> = RwLock::new(None);
static SHARD: RwLock<Option<String>> = RwLock::new(None);

/// Sends every line to `sink` instead of stdout/stderr (tests capture lines with it); None restores the console.
pub fn set_sink(sink: Option<Sink>) {
    *SINK.write().unwrap_or_else(|e| e.into_inner()) = sink;
}

/// The shard id printed as `[Shard N] ` (the JS reads it from the worker's SHARDS; SHARDS and SHARD_ID in the
/// environment still apply when this is not set).
pub fn set_shard_id(id: Option<String>) {
    *SHARD.write().unwrap_or_else(|e| e.into_inner()) = id;
}

static COLOR: LazyLock<bool> = LazyLock::new(|| match std::env::var("FORCE_COLOR") {
    Ok(v) => !matches!(v.as_str(), "0" | "false"),
    Err(_) => std::io::stdout().is_terminal() && std::env::var("TERM").map_or(true, |t| t != "dumb"),
});

fn paint(code: &str, s: &str) -> String {
    if *COLOR { format!("\x1b[{code}m{s}\x1b[39m") } else { s.to_string() }
}

fn timestamp() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

fn shard_prefix() -> String {
    let set = SHARD.read().unwrap_or_else(|e| e.into_inner()).clone();
    let id = set.or_else(|| std::env::var("SHARDS").ok()).or_else(|| std::env::var("SHARD_ID").ok());
    match id {
        Some(id) if !id.is_empty() => paint("90", &format!("[Shard {id}] ")),
        _ => String::new(),
    }
}

/// C0 controls and DEL as escapes (`\n`, `\r`, `\x1b`), so a value can neither start a forged line nor inject
/// terminal codes.
pub fn escape_controls(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\u{0}'..='\u{1f}' | '\u{7f}' => out.push_str(&format!("\\x{:02x}", c as u32)),
            _ => out.push(c),
        }
    }
    out
}

fn format_arg(a: &LogArg) -> String {
    match a {
        LogArg::Error { message, .. } => escape_controls(message),
        LogArg::Str(s) => escape_controls(s),
        LogArg::Json(v) => inspect(v),
        LogArg::Num(n) => escape_controls(&js::number_to_string(*n)),
        LogArg::Bool(b) => b.to_string(),
        LogArg::Null => "null".into(),
        LogArg::Undefined => "undefined".into(),
    }
}

/// The text of one log line, without colour codes when colour is off.
pub fn format_line(level: Level, args: &[LogArg]) -> String {
    let ts = paint("90", &format!("[{}]", timestamp()));
    let lvl = paint(level.color(), &format!("[{}]", level.label()));
    let parts: Vec<String> = args.iter().map(format_arg).collect();
    format!("{ts} {lvl} {}{}", shard_prefix(), parts.join(" "))
}

/// The lines a [`capture`] / [`capture_sync`] scope collected.
pub type Captured = Arc<Mutex<Vec<(Level, String)>>>;

tokio::task_local! {
    static TASK_CAPTURE: Captured;
}

thread_local! {
    static THREAD_CAPTURE: std::cell::RefCell<Option<Captured>> = const { std::cell::RefCell::new(None) };
}

/// Runs `fut` with its log lines captured instead of printed (or sent to the [`set_sink`] sink), and returns them with
/// its output. Only lines logged by `fut` itself (its task, not tasks it spawns) are captured, so parallel tests in
/// one binary do not see each other's lines.
pub async fn capture<F: std::future::Future>(fut: F) -> (F::Output, Vec<(Level, String)>) {
    let lines: Captured = Arc::new(Mutex::new(Vec::new()));
    let out = TASK_CAPTURE.scope(lines.clone(), fut).await;
    let v = std::mem::take(&mut *lines.lock().unwrap_or_else(|e| e.into_inner()));
    (out, v)
}

static GLOBAL_CAPTURE: Mutex<Option<Captured>> = Mutex::new(None);
static GLOBAL_CAPTURE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Like [`capture`], but also collects the lines of tasks `fut` spawns (scheduler timers, `with_timeout`, ...) and of
/// every other task or thread of the process that is not inside a [`capture`] scope, while `fut` runs. Global
/// captures run one at a time (a second one waits), so a test binary should not log from unrelated tests meanwhile.
pub async fn capture_global<F: std::future::Future>(fut: F) -> (F::Output, Vec<(Level, String)>) {
    let _one = GLOBAL_CAPTURE_LOCK.lock().await;
    let lines: Captured = Arc::new(Mutex::new(Vec::new()));
    *GLOBAL_CAPTURE.lock().unwrap_or_else(|e| e.into_inner()) = Some(lines.clone());
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            *GLOBAL_CAPTURE.lock().unwrap_or_else(|e| e.into_inner()) = None;
        }
    }
    let reset = Reset;
    let out = TASK_CAPTURE.scope(lines.clone(), fut).await;
    drop(reset);
    let v = std::mem::take(&mut *lines.lock().unwrap_or_else(|e| e.into_inner()));
    (out, v)
}

/// [`capture`] for synchronous code on this thread.
pub fn capture_sync<T>(f: impl FnOnce() -> T) -> (T, Vec<(Level, String)>) {
    let lines: Captured = Arc::new(Mutex::new(Vec::new()));
    let prev = THREAD_CAPTURE.with(|c| c.replace(Some(lines.clone())));
    let out = f();
    THREAD_CAPTURE.with(|c| *c.borrow_mut() = prev);
    let v = std::mem::take(&mut *lines.lock().unwrap_or_else(|e| e.into_inner()));
    (out, v)
}

fn emit(level: Level, line: &str) {
    let captured = TASK_CAPTURE
        .try_with(Clone::clone)
        .ok()
        .or_else(|| THREAD_CAPTURE.with(|c| c.borrow().clone()))
        .or_else(|| GLOBAL_CAPTURE.lock().unwrap_or_else(|e| e.into_inner()).clone());
    if let Some(c) = captured {
        c.lock().unwrap_or_else(|e| e.into_inner()).push((level, line.to_string()));
        return;
    }
    if let Some(sink) = SINK.read().unwrap_or_else(|e| e.into_inner()).as_ref() {
        sink(level, line);
        return;
    }
    if level.to_stderr() {
        let _ = writeln!(std::io::stderr().lock(), "{line}");
    } else {
        let _ = writeln!(std::io::stdout().lock(), "{line}");
    }
}

/// `logger.info`.
pub fn info(args: &[LogArg]) {
    emit(Level::Info, &format_line(Level::Info, args));
}

/// `logger.warn`.
pub fn warn(args: &[LogArg]) {
    emit(Level::Warn, &format_line(Level::Warn, args));
}

/// `logger.error`: the line, then the stack of every Error argument.
pub fn error(args: &[LogArg]) {
    emit(Level::Error, &format_line(Level::Error, args));
    for a in args {
        if let LogArg::Error { stack: Some(stack), .. } = a {
            emit(Level::Error, &paint("31", stack));
        }
    }
}

/// `logger.debug`: dropped when NODE_ENV is 'production'.
pub fn debug(args: &[LogArg]) {
    if std::env::var("NODE_ENV").is_ok_and(|v| v == "production") {
        return;
    }
    emit(Level::Debug, &format_line(Level::Debug, args));
}

/// `logger.success`.
pub fn success(args: &[LogArg]) {
    emit(Level::Success, &format_line(Level::Success, args));
}

/// `logger.info(a, b, …)`: each argument is anything `Into<LogArg>`.
#[macro_export]
macro_rules! log_info { ($($a:expr),* $(,)?) => { $crate::logger::info(&[$($crate::logger::LogArg::from($a)),*]) }; }
/// `logger.warn(a, b, …)`.
#[macro_export]
macro_rules! log_warn { ($($a:expr),* $(,)?) => { $crate::logger::warn(&[$($crate::logger::LogArg::from($a)),*]) }; }
/// `logger.error(a, b, …)`.
#[macro_export]
macro_rules! log_error { ($($a:expr),* $(,)?) => { $crate::logger::error(&[$($crate::logger::LogArg::from($a)),*]) }; }
/// `logger.debug(a, b, …)`.
#[macro_export]
macro_rules! log_debug { ($($a:expr),* $(,)?) => { $crate::logger::debug(&[$($crate::logger::LogArg::from($a)),*]) }; }
/// `logger.success(a, b, …)`.
#[macro_export]
macro_rules! log_success { ($($a:expr),* $(,)?) => { $crate::logger::success(&[$($crate::logger::LogArg::from($a)),*]) }; }

// ── util.inspect for JSON values ───────────────────────────────────────────────

const DEPTH: usize = 3;
const COMPACT: usize = 3;
const MAX_ARRAY: usize = 100;
const MAX_STRING: usize = 10_000;

struct Ctx {
    current_depth: usize,
    indent: usize,
}

fn str_escape(s: &str) -> String {
    let (quote, escape_single) = if !s.contains('\'') {
        ('\'', true)
    } else if !s.contains('"') {
        ('"', false)
    } else if !s.contains('`') && !s.contains("${") {
        ('`', false)
    } else {
        ('\'', true)
    };
    let mut out = String::with_capacity(s.len() + 2);
    out.push(quote);
    for c in s.chars() {
        match c {
            '\'' if escape_single => out.push_str("\\'"),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\u{b}' => out.push_str("\\x0B"),
            '\u{c}' => out.push_str("\\f"),
            '\r' => out.push_str("\\r"),
            '\u{0}'..='\u{1f}' | '\u{7f}'..='\u{9f}' => out.push_str(&format!("\\x{:02X}", c as u32)),
            _ => out.push(c),
        }
    }
    out.push(quote);
    out
}

fn is_ident_key(k: &str) -> bool {
    let mut chars = k.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_') && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn format_number(v: &serde_json::Number) -> String {
    if let Some(i) = v.as_i64() {
        return i.to_string();
    }
    if let Some(u) = v.as_u64() {
        return u.to_string();
    }
    js::number_to_string(v.as_f64().unwrap_or(f64::NAN))
}

fn format_value(ctx: &mut Ctx, v: &Value, recurse: usize) -> String {
    match v {
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => format_number(n),
        Value::String(s) => {
            let n = s.chars().count();
            if n > MAX_STRING {
                let cut: String = s.chars().take(MAX_STRING).collect();
                format!("{}... {} more character{}", str_escape(&cut), n - MAX_STRING, if n - MAX_STRING > 1 { "s" } else { "" })
            } else {
                str_escape(s)
            }
        }
        Value::Array(a) => {
            if a.is_empty() {
                return "[]".into();
            }
            if recurse > DEPTH {
                return "[Array]".into();
            }
            let recurse = recurse + 1;
            ctx.current_depth = recurse;
            let mut out = Vec::new();
            for x in a.iter().take(MAX_ARRAY) {
                ctx.indent += 2;
                out.push(format_value(ctx, x, recurse));
                ctx.indent -= 2;
            }
            if a.len() > MAX_ARRAY {
                let more = a.len() - MAX_ARRAY;
                out.push(format!("... {more} more item{}", if more > 1 { "s" } else { "" }));
            }
            let all_numbers = a.iter().take(out.len()).all(Value::is_number);
            reduce(ctx, out, ("[", "]"), true, recurse, all_numbers, a.len() > MAX_ARRAY)
        }
        Value::Object(o) => {
            if o.is_empty() {
                return "{}".into();
            }
            if recurse > DEPTH {
                return "[Object]".into();
            }
            let recurse = recurse + 1;
            ctx.current_depth = recurse;
            let mut out = Vec::new();
            for (k, x) in o {
                ctx.indent += 2;
                let s = format_value(ctx, x, recurse);
                ctx.indent -= 2;
                let key = if is_ident_key(k) { k.clone() } else { str_escape(k) };
                out.push(format!("{key}: {s}"));
            }
            reduce(ctx, out, ("{", "}"), false, recurse, false, false)
        }
    }
}

fn group_array_elements(output: Vec<String>, all_numbers: bool, has_more: bool) -> Vec<String> {
    let mut total = 0usize;
    let mut max_len = 0usize;
    let output_len = if has_more { output.len() - 1 } else { output.len() };
    let sep = 2;
    let data_len: Vec<usize> = output[..output_len].iter().map(|s| js::utf16_len(s)).collect();
    for &l in &data_len {
        total += l + sep;
        max_len = max_len.max(l);
    }
    let actual_max = max_len + sep;
    // breakLength is Infinity, so only the length-ratio test matters.
    if (total as f64 / actual_max as f64) > 5.0 || max_len <= 6 {
        let average_bias = (actual_max as f64 - total as f64 / output.len() as f64).sqrt();
        let biased_max = (actual_max as f64 - 3.0 - average_bias).max(1.0);
        let columns = crate::text::js_round((2.5 * biased_max * output_len as f64).sqrt() / biased_max).min((COMPACT * 4) as f64).min(15.0);
        if columns.is_nan() || columns <= 1.0 {
            return output;
        }
        let columns = columns as usize;
        let mut max_line = Vec::with_capacity(columns);
        for i in 0..columns {
            let mut line = 0;
            let mut j = i;
            while j < output.len() {
                if let Some(&d) = data_len.get(j) {
                    line = line.max(d);
                }
                j += columns;
            }
            max_line.push(line + sep);
        }
        let mut tmp = Vec::new();
        let mut i = 0;
        while i < output_len {
            let max = (i + columns).min(output_len);
            let mut s = String::new();
            let mut j = i;
            while j < max - 1 {
                let cell = format!("{}, ", output[j]);
                s.push_str(&pad(&cell, max_line[j - i], all_numbers));
                j += 1;
            }
            if all_numbers {
                s.push_str(&pad(&output[j], max_line[j - i] - sep, true));
            } else {
                s.push_str(&output[j]);
            }
            tmp.push(s);
            i += columns;
        }
        if has_more {
            tmp.push(output[output_len].clone());
        }
        return tmp;
    }
    output
}

fn pad(s: &str, width: usize, start: bool) -> String {
    let len = js::utf16_len(s);
    if len >= width {
        return s.to_string();
    }
    let fill = " ".repeat(width - len);
    if start { format!("{fill}{s}") } else { format!("{s}{fill}") }
}

fn reduce(ctx: &Ctx, output: Vec<String>, braces: (&str, &str), array: bool, recurse: usize, all_numbers: bool, has_more: bool) -> String {
    let entries = output.len();
    let output = if array && entries > 6 { group_array_elements(output, all_numbers, has_more) } else { output };
    if ctx.current_depth - recurse < COMPACT && entries == output.len() {
        let joined = output.join(", ");
        if !joined.contains('\n') {
            return format!("{} {joined} {}", braces.0, braces.1);
        }
    }
    let indentation = format!("\n{}", " ".repeat(ctx.indent));
    format!("{}{indentation}  {}{indentation}{}", braces.0, output.join(&format!(",{indentation}  ")), braces.1)
}

/// `util.inspect(value, { depth: 3, breakLength: Infinity })` for a JSON value.
pub fn inspect(v: &Value) -> String {
    let mut ctx = Ctx { current_depth: 0, indent: 0 };
    format_value(&mut ctx, v, 0)
}
