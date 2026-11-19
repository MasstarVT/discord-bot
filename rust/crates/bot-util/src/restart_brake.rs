//! Port of `src/utils/restartBrake.js`: paces bot starts so a crash loop cannot use up Discord's 1000 IDENTIFY calls
//! a day. The history lives in `<STATE_DIR>/restart-brake.json` as `{"starts":[ms, ...]}`, the same file the JS
//! deployment writes, so either bot reads the other's history.

use std::path::{Path, PathBuf};

const WINDOW_MS: f64 = 60.0 * 60_000.0;
const FREE_STARTS: u64 = 5;
const MAX_DELAY_MS: u64 = 30 * 60_000;
/// The state file's name.
pub const FILE_NAME: &str = "restart-brake.json";

/// Delay for the n-th start within the last hour: none up to 5, then 2^(n-5) minutes capped at 30 minutes.
pub fn brake_delay_ms(starts: u64) -> u64 {
    brake_delay_ms_f64(starts as f64) as u64
}

/// `brakeDelayMs` for any number, as the JS computes it.
pub fn brake_delay_ms_f64(starts: f64) -> f64 {
    if starts <= FREE_STARTS as f64 {
        return 0.0;
    }
    (MAX_DELAY_MS as f64).min(2f64.powf(starts - FREE_STARTS as f64) * 60_000.0)
}

/// What [`record_start`] did.
#[derive(Debug, Clone, PartialEq)]
pub struct RecordStart {
    /// Starts in the last hour, this one included.
    pub starts: u64,
    /// How long to wait before connecting.
    pub delay_ms: u64,
    /// The file written; None when no location was writable (the brake is then off).
    pub file: Option<PathBuf>,
    pub warning: Option<String>,
}

/// Reads the start history; anything unreadable is an empty history. Non-numbers are dropped.
pub fn read_starts(file: &Path) -> Vec<f64> {
    let Ok(text) = std::fs::read_to_string(file) else { return Vec::new() };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else { return Vec::new() };
    match v.get("starts").and_then(|s| s.as_array()) {
        Some(a) => a.iter().filter_map(serde_json::Value::as_f64).filter(|n| n.is_finite()).collect(),
        None => Vec::new(),
    }
}

/// `JSON.stringify({ starts })`.
pub fn starts_json(starts: &[f64]) -> String {
    let items: Vec<String> = starts.iter().map(|n| crate::js::number_to_string(*n)).collect();
    format!("{{\"starts\":[{}]}}", items.join(","))
}

fn write_starts(dir: &Path, starts: &[f64]) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let file = dir.join(FILE_NAME);
    let tmp = dir.join(format!("{FILE_NAME}.tmp"));
    std::fs::write(&tmp, starts_json(starts))?;
    std::fs::rename(&tmp, &file)
}

/// The Node error code (`err.code`) of an I/O error, else its message.
pub fn node_error_code(err: &std::io::Error) -> String {
    #[cfg(unix)]
    if let Some(code) = err.raw_os_error() {
        let name = match code {
            1 => "EPERM",
            2 => "ENOENT",
            5 => "EIO",
            13 => "EACCES",
            17 => "EEXIST",
            20 => "ENOTDIR",
            21 => "EISDIR",
            22 => "EINVAL",
            24 => "EMFILE",
            28 => "ENOSPC",
            30 => "EROFS",
            36 => "ENAMETOOLONG",
            122 => "EDQUOT",
            _ => "",
        };
        if !name.is_empty() {
            return name.into();
        }
    }
    use std::io::ErrorKind as K;
    match err.kind() {
        K::PermissionDenied => "EPERM".into(),
        K::NotFound => "ENOENT".into(),
        K::AlreadyExists => "EEXIST".into(),
        K::NotADirectory => "ENOTDIR".into(),
        K::IsADirectory => "EISDIR".into(),
        K::ReadOnlyFilesystem => "EROFS".into(),
        K::StorageFull => "ENOSPC".into(),
        K::InvalidInput => "EINVAL".into(),
        _ => err.to_string(),
    }
}

/// The fallback directory when `STATE_DIR` is not writable (`os.tmpdir()/discord-bot-state`).
pub fn fallback_dir() -> PathBuf {
    std::env::temp_dir().join("discord-bot-state")
}

/// Records this start in `<state_dir>/restart-brake.json`, prunes entries older than an hour, and returns how long
/// to wait before connecting to Discord. When `state_dir` is not writable the history goes to the OS temp dir.
pub fn record_start(state_dir: &Path, now_ms: f64) -> RecordStart {
    record_start_with_fallback(state_dir, &fallback_dir(), now_ms)
}

/// [`record_start`] with an explicit fallback directory.
pub fn record_start_with_fallback(state_dir: &Path, fallback: &Path, now_ms: f64) -> RecordStart {
    let mut warning: Option<String> = None;
    for dir in [state_dir, fallback] {
        let file = dir.join(FILE_NAME);
        let mut starts: Vec<f64> = read_starts(&file).into_iter().filter(|&t| t > now_ms - WINDOW_MS && t <= now_ms).collect();
        starts.push(now_ms);
        match write_starts(dir, &starts) {
            Ok(()) => {
                let n = starts.len() as u64;
                return RecordStart { starts: n, delay_ms: brake_delay_ms(n), file: Some(file), warning };
            }
            Err(e) => warning = Some(format!("Could not write {} ({})", file.display(), node_error_code(&e))),
        }
    }
    RecordStart {
        starts: 1,
        delay_ms: 0,
        file: None,
        warning: Some(format!("{}; restart brake disabled", warning.unwrap_or_default())),
    }
}
