//! The logger server.js takes (`opts.logger`, default the bot's logger): info, warn, error (with an error argument) and
//! success. Injectable so tests can read the lines a request wrote.

use std::sync::Mutex;

use bot_util::logger::LogArg;

/// The four calls server.js and discordApi.js make on their logger.
pub trait DashLogger: Send + Sync {
    fn info(&self, message: &str);
    fn warn(&self, message: &str);
    /// `logger.error(message, err)`: `err` is the error's message when there is one.
    fn error(&self, message: &str, err: Option<&str>);
    fn success(&self, message: &str);
}

/// The bot's logger (src/utils/logger.js through bot_util::logger).
#[derive(Debug, Default, Clone, Copy)]
pub struct BotLogger;

impl DashLogger for BotLogger {
    fn info(&self, message: &str) {
        bot_util::logger::info(&[LogArg::from(message)]);
    }

    fn warn(&self, message: &str) {
        bot_util::logger::warn(&[LogArg::from(message)]);
    }

    fn error(&self, message: &str, err: Option<&str>) {
        match err {
            Some(e) => bot_util::logger::error(&[LogArg::from(message), LogArg::Error { message: e.to_string(), stack: None }]),
            None => bot_util::logger::error(&[LogArg::from(message)]),
        }
    }

    fn success(&self, message: &str) {
        bot_util::logger::success(&[LogArg::from(message)]);
    }
}

/// A logger that keeps every line (`"<level> <message>[ <error>]"`), as the JS tests' recordingLogger does.
#[derive(Debug, Default)]
pub struct RecordingLogger {
    lines: Mutex<Vec<String>>,
}

impl RecordingLogger {
    pub fn new() -> Self {
        Self::default()
    }

    /// The lines so far.
    pub fn lines(&self) -> Vec<String> {
        self.lines.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
    }

    fn push(&self, line: String) {
        self.lines.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(line);
    }
}

impl DashLogger for RecordingLogger {
    fn info(&self, message: &str) {
        self.push(format!("info {message}"));
    }

    fn warn(&self, message: &str) {
        self.push(format!("warn {message}"));
    }

    fn error(&self, message: &str, err: Option<&str>) {
        match err {
            Some(e) => self.push(format!("error {message} {e}")),
            None => self.push(format!("error {message}")),
        }
    }

    fn success(&self, message: &str) {
        self.push(format!("success {message}"));
    }
}
