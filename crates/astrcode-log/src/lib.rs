//! astrcode logging system.
//!
//! Initializes the global [`tracing`] subscriber with two layers:
//!
//! | Layer | Output | Default level | Details |
//! |---|---|---|---|
//! | stderr | terminal | `info` | Human‑readable |
//! | file   | `~/.astrcode/logs/astrcode-YYYY-MM-DD.log` | `debug` | With file/line info |
//!
//! Both levels can be overridden via environment variables:
//!
//! - `ASTRCODE_LOG` – overrides the stderr level.
//! - `ASTRCODE_LOG_FILE` – overrides the file level.
//!
//! # Examples
//!
//! ```no_run
//! use astrcode_log::{init, init_with, LogOptions};
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

use std::path::PathBuf;

use tracing_subscriber::{
    filter::EnvFilter,
    layer::SubscriberExt,
    util::SubscriberInitExt,
    Layer, Registry,
};

/// Default log directory: `~/.astrcode/logs/`.
pub fn default_log_dir() -> PathBuf {
    astrcode_support::hostpaths::astrcode_dir().join("logs")
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
}

impl Default for LogOptions {
    fn default() -> Self {
        Self {
            log_dir: default_log_dir(),
            stderr_filter: "info".into(),
            file_filter: "debug".into(),
            file_enabled: true,
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
    let stderr_filter = std::env::var("ASTRCODE_LOG")
        .unwrap_or_else(|_| opts.stderr_filter);

    let stderr_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_ansi(true)
        .with_target(true)
        .with_level(true)
        .with_filter(EnvFilter::new(&stderr_filter));

    let subscriber = Registry::default().with(stderr_layer);

    if opts.file_enabled {
        std::fs::create_dir_all(&opts.log_dir)
            .expect("failed to create log directory; check ~/.astrcode permissions");

        let file_filter = std::env::var("ASTRCODE_LOG_FILE")
            .unwrap_or_else(|_| opts.file_filter);

        let file_appender = tracing_appender::rolling::daily(&opts.log_dir, "astrcode");
        let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

        let file_layer = tracing_subscriber::fmt::layer()
            .with_writer(non_blocking)
            .with_ansi(false)
            .with_target(true)
            .with_level(true)
            .with_file(true)
            .with_line_number(true)
            .with_filter(EnvFilter::new(&file_filter));

        subscriber.with(file_layer).init();
        guard
    } else {
        subscriber.init();
        tracing_appender::non_blocking(std::io::sink()).1
    }
}
