//! astrcode logging system.
//!
//! Initializes the global [`tracing`] subscriber with two layers:
//!
//! | Layer | Output | Default level | Details |
//! |---|---|---|---|
//! | stderr | terminal | `info` | Human‑readable |
//! | file   | `~/.astrcode/logs/astrcode-YYYYMMDD-HHMMSS-PID.log` | `debug` | With file/line info |
//!
//! Both levels can be overridden via environment variables:
//!
//! - `ASTRCODE_LOG` – overrides the stderr level.
//! - `ASTRCODE_LOG_FILE` – overrides the file level.
//!
//! File logs are retained for 30 days. `latest.logpath` contains the path to
//! the newest process log.
//!
//! # Examples
//!
//! ```no_run
//! use astrcode_log::{LogOptions, init, init_with};
//!
//! // Default settings (info on stderr, debug to file).
//! let _guard = init();
//!
//! // Custom settings.
//! let guard = init_with(LogOptions {
//!     file_enabled: false,
//!     ..LogOptions::default()
//! });
//! ```

use std::{
    fs::{self, OpenOptions},
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use chrono::Local;
use tracing_subscriber::{
    Layer, Registry, filter::EnvFilter, layer::SubscriberExt, util::SubscriberInitExt,
};

const LOG_FILE_PREFIX: &str = "astrcode-";
const LOG_FILE_EXTENSION: &str = "log";
const LATEST_LOG_PATH_FILE: &str = "latest.logpath";
const DEFAULT_RETENTION: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// Default log directory: `~/.astrcode/logs/`.
pub fn default_log_dir() -> PathBuf {
    astrcode_support::hostpaths::logs_dir()
}

/// Options that control logging initialisation.
#[derive(Debug, Clone)]
pub struct LogOptions {
    /// Directory where log files are written.
    pub log_dir: PathBuf,
    /// EnvFilter directive for stderr output (e.g. `"info,astrcode_server=debug"`).
    pub stderr_filter: String,
    /// EnvFilter directive for file output.
    pub file_filter: String,
    /// Whether to enable file logging at all.
    pub file_enabled: bool,
    /// Whether to enable stderr logging. Disable for TUI mode to avoid
    /// corrupting the terminal UI.
    pub stderr_enabled: bool,
}

impl Default for LogOptions {
    fn default() -> Self {
        Self {
            log_dir: default_log_dir(),
            stderr_filter: "info".into(),
            file_filter: "info,astrcode=debug".into(),
            file_enabled: true,
            stderr_enabled: true,
        }
    }
}

/// Initialise the global tracing subscriber with default [`LogOptions`].
///
/// Returns a [`tracing_appender::non_blocking::WorkerGuard`] that **must** be kept
/// alive for the lifetime of the process (holding it in `main()` is the intended
/// pattern). Dropping the guard flushes and shuts down the non‑blocking file writer.
///
/// # Panics
///
/// Panics if `init()` or `init_with()` has already been called in this process
/// (tracing‑subscriber permits only a single global initialisation).
pub fn init() -> tracing_appender::non_blocking::WorkerGuard {
    init_with(LogOptions::default())
}

/// Initialise the global tracing subscriber with custom [`LogOptions`].
///
/// See [`init()`] for lifetime and panic caveats.
pub fn init_with(opts: LogOptions) -> tracing_appender::non_blocking::WorkerGuard {
    let subscriber = Registry::default();

    let stderr_layer = if opts.stderr_enabled {
        let stderr_filter = std::env::var("ASTRCODE_LOG").unwrap_or(opts.stderr_filter);
        Some(
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::stderr)
                .with_ansi(true)
                .with_target(true)
                .with_level(true)
                .with_filter(EnvFilter::new(&stderr_filter)),
        )
    } else {
        None
    };

    let subscriber = subscriber.with(stderr_layer);

    if opts.file_enabled {
        if let Err(e) = fs::create_dir_all(&opts.log_dir) {
            tracing::error!("failed to create log directory: {e}");
            subscriber.init();
            return tracing_appender::non_blocking(std::io::sink()).1;
        }

        let file_filter = std::env::var("ASTRCODE_LOG_FILE").unwrap_or(opts.file_filter);

        let log_path = next_log_path(&opts.log_dir);
        let log_file = match open_log_file(&log_path) {
            Ok(f) => f,
            Err(e) => {
                tracing::error!("failed to create log file: {e}");
                subscriber.init();
                return tracing_appender::non_blocking(std::io::sink()).1;
            },
        };
        let (non_blocking, guard) = tracing_appender::non_blocking(log_file);

        let file_layer = tracing_subscriber::fmt::layer()
            .with_writer(non_blocking)
            .with_ansi(false)
            .with_target(true)
            .with_level(true)
            .with_file(true)
            .with_line_number(true)
            .with_filter(EnvFilter::new(&file_filter));

        subscriber.with(file_layer).init();
        remember_latest_log(&opts.log_dir, &log_path);
        cleanup_old_logs(&opts.log_dir, DEFAULT_RETENTION);
        tracing::debug!(log_file = %log_path.display(), "file logging initialized");
        guard
    } else {
        subscriber.init();
        tracing_appender::non_blocking(std::io::sink()).1
    }
}

fn next_log_path(log_dir: &Path) -> PathBuf {
    let timestamp = Local::now().format("%Y%m%d-%H%M%S");
    let stem = format!("{LOG_FILE_PREFIX}{timestamp}-{}", std::process::id());
    let mut path = log_dir.join(format!("{stem}.{LOG_FILE_EXTENSION}"));
    let mut suffix = 1;

    while path.exists() {
        path = log_dir.join(format!("{stem}-{suffix}.{LOG_FILE_EXTENSION}"));
        suffix += 1;
    }

    path
}

fn open_log_file(path: &Path) -> std::io::Result<std::fs::File> {
    let mut opts = OpenOptions::new();
    opts.create_new(true).write(true);

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }

    opts.open(path)
}

fn remember_latest_log(log_dir: &Path, log_path: &Path) {
    let pointer = log_dir.join(LATEST_LOG_PATH_FILE);
    if let Err(error) = fs::write(&pointer, log_path.display().to_string()) {
        tracing::warn!(
            path = %pointer.display(),
            "failed to update latest log pointer: {error}"
        );
    }
}

fn cleanup_old_logs(log_dir: &Path, retention: Duration) {
    let cutoff = SystemTime::now()
        .checked_sub(retention)
        .unwrap_or(SystemTime::UNIX_EPOCH);
    cleanup_logs_before(log_dir, cutoff);
}

fn cleanup_logs_before(log_dir: &Path, cutoff: SystemTime) {
    let entries = match fs::read_dir(log_dir) {
        Ok(entries) => entries,
        Err(error) => {
            tracing::warn!(
                path = %log_dir.display(),
                "failed to read log directory for cleanup: {error}"
            );
            return;
        },
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !is_astrcode_log_file(&path) {
            continue;
        }

        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        if modified >= cutoff {
            continue;
        }

        if let Err(error) = fs::remove_file(&path) {
            tracing::warn!(path = %path.display(), "failed to remove old log file: {error}");
        }
    }
}

fn is_astrcode_log_file(path: &Path) -> bool {
    let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };

    is_process_log_file(path, file_name) || is_legacy_daily_log_file(file_name)
}

fn is_process_log_file(path: &Path, file_name: &str) -> bool {
    file_name.starts_with(LOG_FILE_PREFIX)
        && path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension == LOG_FILE_EXTENSION)
}

fn is_legacy_daily_log_file(file_name: &str) -> bool {
    let Some(date) = file_name.strip_prefix("astrcode.") else {
        return false;
    };
    let bytes = date.as_bytes();

    date.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| index == 4 || index == 7 || byte.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use std::{
        fs::{self, File},
        path::PathBuf,
        thread,
        time::{Duration, SystemTime},
    };

    use super::*;

    #[test]
    fn next_log_path_does_not_reuse_existing_file() {
        let dir = test_log_dir("unique");
        fs::create_dir_all(&dir).unwrap();

        let first = next_log_path(&dir);
        File::create(&first).unwrap();
        let second = next_log_path(&dir);

        assert_ne!(first, second);
        assert!(
            second
                .file_name()
                .unwrap()
                .to_string_lossy()
                .ends_with("-1.log")
        );
        cleanup_test_dir(&dir);
    }

    #[test]
    fn latest_log_pointer_records_current_log_path() {
        let dir = test_log_dir("latest");
        fs::create_dir_all(&dir).unwrap();
        let log_path = dir.join("astrcode-20260504-120000-42.log");

        remember_latest_log(&dir, &log_path);

        let latest = fs::read_to_string(dir.join(LATEST_LOG_PATH_FILE)).unwrap();
        assert_eq!(latest, log_path.display().to_string());
        cleanup_test_dir(&dir);
    }

    #[test]
    fn cleanup_old_logs_keeps_recent_and_foreign_files() {
        let dir = test_log_dir("cleanup");
        fs::create_dir_all(&dir).unwrap();
        let old_log = dir.join("astrcode-20240101-000000-1.log");
        let legacy_log = dir.join("astrcode.2024-01-01");
        let recent_log = dir.join("astrcode-20260504-120000-1.log");
        let foreign = dir.join("other.log");
        File::create(&old_log).unwrap();
        File::create(&legacy_log).unwrap();
        let cutoff = SystemTime::now();
        thread::sleep(Duration::from_millis(20));
        File::create(&recent_log).unwrap();
        File::create(&foreign).unwrap();

        cleanup_logs_before(&dir, cutoff);

        assert!(!old_log.exists());
        assert!(!legacy_log.exists());
        assert!(recent_log.exists());
        assert!(foreign.exists());
        cleanup_test_dir(&dir);
    }

    fn test_log_dir(name: &str) -> PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "astrcode-log-test-{name}-{}-{:?}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        dir
    }

    fn cleanup_test_dir(dir: &Path) {
        if dir.starts_with(std::env::temp_dir()) {
            let _ = fs::remove_dir_all(dir);
        }
    }
}
