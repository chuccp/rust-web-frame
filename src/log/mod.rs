//! Logging — the "Zap" of this framework, on top of `tracing`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// Config section that holds the logging settings.
pub const LOG_CONFIG_KEY: &str = "log";

/// Log output format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    /// Human readable, coloured when the terminal supports it.
    #[default]
    Text,
    /// One JSON object per line, for log collectors.
    Json,
}

/// Logging configuration, read from the `log` section of the config file.
///
/// ```yaml
/// log:
///   level: debug
///   path: ./logs/app.log
///   format: text
///   ansi: true
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct LogConfig {
    /// A `tracing` filter directive, e.g. `info`, `debug`, `my_app=debug,tower=warn`.
    pub level: String,
    /// When set, logs are also appended to a rotating file at this path.
    pub path: Option<PathBuf>,
    /// Console format.
    pub format: LogFormat,
    /// Whether to colour console output.
    pub ansi: bool,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            level: String::from("info"),
            path: None,
            format: LogFormat::Text,
            ansi: true,
        }
    }
}

impl LogConfig {
    /// A configuration at `level`.
    pub fn at_level(level: impl Into<String>) -> Self {
        Self {
            level: level.into(),
            ..Self::default()
        }
    }

    /// Also writes logs to `path`, rotating daily.
    pub fn with_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.path = Some(path.into());
        self
    }

    /// Switches the console to JSON lines.
    pub fn with_json(mut self) -> Self {
        self.format = LogFormat::Json;
        self
    }
}

/// Keeps the file-appender worker alive.
///
/// Dropping it flushes and shuts the writer thread down, after which every line bound for
/// the file is silently discarded. Applications never hold one: [`init_logger`] keeps the
/// guard for the lifetime of the process.
pub struct LogGuard {
    _file: Option<tracing_appender::non_blocking::WorkerGuard>,
}

/// The process-wide guard. It is never dropped, which is the point.
static GUARD: std::sync::OnceLock<LogGuard> = std::sync::OnceLock::new();

/// Initialises the global logger and returns the guard that keeps its file appender
/// alive.
///
/// Safe to call more than once: the first call installs the subscriber and later calls
/// return the guard installed by the first, ignoring their argument.
///
/// The guard belongs to the process rather than to the caller. That is deliberate — a
/// guard stored in a local would shut the writer thread down when that local went out of
/// scope, so `log.path` would appear to work during startup and then silently drop every
/// line written afterwards.
///
/// ```no_run
/// # use rust_web_frame::log::{init_logger, LogConfig};
/// init_logger(LogConfig::at_level("debug"));
/// tracing::info!("logger ready");
/// ```
pub fn init_logger(config: LogConfig) -> &'static LogGuard {
    GUARD.get_or_init(|| install_logger(config))
}

/// Builds the subscriber and the guard that keeps it writing.
fn install_logger(config: LogConfig) -> LogGuard {
    let filter = EnvFilter::try_new(&config.level).unwrap_or_else(|_| EnvFilter::new("info"));

    let file_guard = config.path.as_ref().map(|path| {
        let directory = path.parent().filter(|p| !p.as_os_str().is_empty());
        let file_name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| String::from("app.log"));
        let appender = match directory {
            Some(directory) => {
                if let Err(err) = std::fs::create_dir_all(directory) {
                    eprintln!(
                        "rust-web-frame: cannot create the log directory {}: {err}",
                        directory.display()
                    );
                }
                tracing_appender::rolling::daily(directory, file_name)
            }
            None => tracing_appender::rolling::daily(".", file_name),
        };
        let (writer, guard) = tracing_appender::non_blocking(appender);
        (writer, guard)
    });

    let registry = tracing_subscriber::registry().with(filter);

    match (&config.format, &file_guard) {
        (LogFormat::Json, Some((writer, _))) => {
            let console = tracing_subscriber::fmt::layer()
                .json()
                .with_ansi(config.ansi);
            let file = tracing_subscriber::fmt::layer()
                .json()
                .with_ansi(false)
                .with_writer(writer.clone());
            let _ = registry.with(console).with(file).try_init();
        }
        (LogFormat::Json, None) => {
            let console = tracing_subscriber::fmt::layer()
                .json()
                .with_ansi(config.ansi);
            let _ = registry.with(console).try_init();
        }
        (LogFormat::Text, Some((writer, _))) => {
            let console = tracing_subscriber::fmt::layer().with_ansi(config.ansi);
            let file = tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(writer.clone());
            let _ = registry.with(console).with(file).try_init();
        }
        (LogFormat::Text, None) => {
            let console = tracing_subscriber::fmt::layer().with_ansi(config.ansi);
            let _ = registry.with(console).try_init();
        }
    }

    LogGuard {
        _file: file_guard.map(|(_, guard)| guard),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_the_quiet_ones() {
        let config = LogConfig::default();
        assert_eq!(config.level, "info");
        assert!(config.path.is_none());
        assert_eq!(config.format, LogFormat::Text);
    }

    #[test]
    fn camel_case_keys_round_trip() {
        let parsed: LogConfig =
            serde_json::from_value(serde_json::json!({"level": "debug", "path": "./logs/app.log"}))
                .unwrap();
        assert_eq!(parsed.level, "debug");
        assert_eq!(
            parsed.path.as_deref(),
            Some(std::path::Path::new("./logs/app.log"))
        );
    }

    #[test]
    fn logger_initialises_without_a_file() {
        // Subsequent calls are no-ops, so this is safe alongside other tests.
        init_logger(LogConfig::at_level("warn"));
        tracing::warn!("this line is swallowed by the test harness");
    }
}
