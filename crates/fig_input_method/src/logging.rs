//! A log file and a level, which is all `fig_log` was used for here.
//!
//! `fig_log` brings `tracing-subscriber` and `tracing-appender` — a registry, a
//! reloadable `EnvFilter` and a writer thread — to a process that at its default
//! level writes nothing. The file location and the `Q_LOG_LEVEL` default match
//! `fig_log`; the line format is plainer.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum Level {
    Error = 0,
    Warn = 1,
    Info = 2,
    Debug = 3,
    Trace = 4,
}

impl Level {
    fn label(self) -> &'static str {
        match self {
            Level::Error => "ERROR",
            Level::Warn => "WARN",
            Level::Info => "INFO",
            Level::Debug => "DEBUG",
            Level::Trace => "TRACE",
        }
    }
}

/// Same default as `fig_log`: only errors, unless `Q_LOG_LEVEL` says otherwise.
static MAX_LEVEL: AtomicU8 = AtomicU8::new(Level::Error as u8);
static SINK: Mutex<Option<File>> = Mutex::new(None);

/// Accepts a bare level (`debug`) and the trailing level of a `tracing` directive
/// (`fig_input_method=debug`), which covers how `Q_LOG_LEVEL` is used in practice.
fn parse_level(raw: &str) -> Option<Level> {
    let value = raw.rsplit(['=', ',']).next()?.trim();
    match value.to_ascii_lowercase().as_str() {
        "error" => Some(Level::Error),
        "warn" => Some(Level::Warn),
        "info" => Some(Level::Info),
        "debug" => Some(Level::Debug),
        "trace" => Some(Level::Trace),
        _ => None,
    }
}

/// Truncates the previous run's file, matching `delete_old_log_file: true`.
pub fn init() {
    if let Some(level) = std::env::var("Q_LOG_LEVEL").ok().as_deref().and_then(parse_level) {
        MAX_LEVEL.store(level as u8, Ordering::Relaxed);
    }

    let Some(path) = crate::paths::log_file_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(file) = OpenOptions::new().create(true).write(true).truncate(true).open(&path) {
        *SINK.lock().unwrap_or_else(|err| err.into_inner()) = Some(file);
    }
}

pub fn enabled(level: Level) -> bool {
    level as u8 <= MAX_LEVEL.load(Ordering::Relaxed)
}

pub fn write(level: Level, args: std::fmt::Arguments<'_>) {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_millis())
        .unwrap_or_default();
    let mut guard = SINK.lock().unwrap_or_else(|err| err.into_inner());
    if let Some(file) = guard.as_mut() {
        let _ = writeln!(file, "{millis} {} {args}", level.label());
    }
}

macro_rules! log_at {
    ($level:expr, $($arg:tt)*) => {{
        let level = $level;
        if $crate::logging::enabled(level) {
            $crate::logging::write(level, format_args!($($arg)*));
        }
    }};
}

macro_rules! log_error {
    ($($arg:tt)*) => { log_at!($crate::logging::Level::Error, $($arg)*) };
}

macro_rules! log_info {
    ($($arg:tt)*) => { log_at!($crate::logging::Level::Info, $($arg)*) };
}

macro_rules! log_debug {
    ($($arg:tt)*) => { log_at!($crate::logging::Level::Debug, $($arg)*) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_parsing_accepts_bare_and_directive_forms() {
        assert_eq!(parse_level("debug"), Some(Level::Debug));
        assert_eq!(parse_level("TRACE"), Some(Level::Trace));
        assert_eq!(parse_level("fig_input_method=info"), Some(Level::Info));
        assert_eq!(parse_level("nonsense"), None);
    }

    #[test]
    fn default_level_only_admits_errors() {
        assert!(enabled(Level::Error));
        assert!(!enabled(Level::Info));
    }
}
