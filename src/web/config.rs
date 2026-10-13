//! Server configuration.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Configuration key under which the server section lives.
pub const SERVER_CONFIG_KEY: &str = "web.server";

/// Default maximum request body size: 10 MiB.
pub const DEFAULT_MAX_BODY_SIZE: i64 = 10 << 20;

/// Default listen port, kept identical to the Go original so that migration is a no-op.
pub const DEFAULT_PORT: u16 = 19009;

/// HTTP server configuration, read from the `web.server` section of the config file.
///
/// ```yaml
/// web:
///   server:
///     port: 8080
///     contextPath: /api
///     locations: ["./public"]
///     page404: index.html
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ServerConfig {
    /// Listen port. Defaults to `19009`.
    pub port: u16,
    /// Prefix applied to every route, e.g. `/api`. Empty by default.
    pub context_path: String,
    /// Directories served as static files when no route matches.
    pub locations: Vec<PathBuf>,
    /// File served for unmatched browser requests — the SPA fallback.
    /// Configured as `page404`, matching the Go original.
    #[serde(rename = "page404")]
    pub not_found_page: Option<PathBuf>,
    /// Maximum request body size in bytes. `0` means the 10 MiB default, `-1` means
    /// unlimited.
    pub max_body_size: i64,
    /// How much of one uploaded file is held in memory before it spills to a temporary
    /// file. `0` means the 1 MiB default.
    pub max_upload_memory: i64,
    /// Seconds to wait for request headers. `0` means 15.
    pub read_header_timeout: u64,
    /// Seconds a whole request may take. `0` means no limit.
    pub read_timeout: u64,
    /// Maximum size of the request head, in bytes. `0` means 16 KiB.
    pub max_header_bytes: usize,
    /// TLS configuration. `None` serves plain HTTP.
    pub ssl: Option<SslConfig>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            port: DEFAULT_PORT,
            context_path: String::new(),
            locations: Vec::new(),
            not_found_page: None,
            max_body_size: 0,
            max_upload_memory: 0,
            read_header_timeout: 0,
            read_timeout: 0,
            max_header_bytes: 0,
            ssl: None,
        }
    }
}

impl ServerConfig {
    /// A configuration listening on `port`.
    pub fn on_port(port: u16) -> Self {
        Self {
            port,
            ..Self::default()
        }
    }

    /// Sets the route prefix.
    pub fn with_context_path(mut self, context_path: impl Into<String>) -> Self {
        self.context_path = context_path.into();
        self
    }

    /// Serves `path` as static files for unmatched requests.
    pub fn with_static_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.locations.push(path.into());
        self
    }

    /// Sets the SPA fallback page.
    pub fn with_not_found_page(mut self, path: impl Into<PathBuf>) -> Self {
        self.not_found_page = Some(path.into());
        self
    }

    /// Reports whether TLS is enabled.
    pub fn ssl_enabled(&self) -> bool {
        self.ssl.as_ref().is_some_and(|ssl| ssl.enabled)
    }

    /// The effective body size limit in bytes; `usize::MAX` means unlimited.
    pub fn effective_max_body_size(&self) -> usize {
        match self.max_body_size {
            n if n < 0 => usize::MAX,
            0 => DEFAULT_MAX_BODY_SIZE as usize,
            n => n as usize,
        }
    }

    /// The effective in-memory budget for one uploaded file, in bytes.
    ///
    /// Anything larger is written to a temporary file instead, so a big upload costs disk
    /// space rather than resident memory.
    #[must_use]
    pub fn effective_max_upload_memory(&self) -> u64 {
        if self.max_upload_memory > 0 {
            self.max_upload_memory as u64
        } else {
            super::multipart::DEFAULT_MAX_UPLOAD_MEMORY
        }
    }

    /// The effective read-header timeout.
    pub fn effective_read_header_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(if self.read_header_timeout > 0 {
            self.read_header_timeout
        } else {
            15
        })
    }

    /// The effective read timeout; `None` means no limit.
    pub fn effective_read_timeout(&self) -> Option<std::time::Duration> {
        (self.read_timeout > 0).then(|| std::time::Duration::from_secs(self.read_timeout))
    }

    /// The effective maximum request head size.
    pub fn effective_max_header_bytes(&self) -> usize {
        if self.max_header_bytes > 0 {
            self.max_header_bytes
        } else {
            16 * 1024
        }
    }
}

/// TLS / HTTPS settings.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SslConfig {
    /// Whether HTTPS is enabled.
    pub enabled: bool,
    /// Path to a PEM certificate chain.
    pub cert_file: Option<PathBuf>,
    /// Path to the matching PEM private key.
    pub key_file: Option<PathBuf>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_go_original() {
        let config = ServerConfig::default();
        assert_eq!(config.port, 19009);
        assert_eq!(config.effective_max_body_size(), 10 << 20);
        assert!(!config.ssl_enabled());
    }

    #[test]
    fn max_body_size_zero_means_default_and_negative_means_unlimited() {
        let mut config = ServerConfig::default();
        assert_eq!(
            config.effective_max_body_size(),
            DEFAULT_MAX_BODY_SIZE as usize
        );

        config.max_body_size = -1;
        assert_eq!(config.effective_max_body_size(), usize::MAX);

        config.max_body_size = 1024;
        assert_eq!(config.effective_max_body_size(), 1024);
    }

    #[test]
    fn upload_memory_defaults_to_one_mebibyte() {
        let mut config = ServerConfig::default();
        assert_eq!(
            config.effective_max_upload_memory(),
            super::super::multipart::DEFAULT_MAX_UPLOAD_MEMORY
        );

        config.max_upload_memory = 4096;
        assert_eq!(config.effective_max_upload_memory(), 4096);
    }

    #[test]
    fn camel_case_keys_round_trip() {
        let json = serde_json::to_value(ServerConfig::default()).unwrap();
        assert!(json.get("contextPath").is_some());
        assert!(
            json.get("page404").is_some(),
            "the wire key keeps the Go spelling"
        );

        let parsed: ServerConfig =
            serde_json::from_value(serde_json::json!({"port": 8080, "contextPath": "/api"}))
                .unwrap();
        assert_eq!(parsed.port, 8080);
        assert_eq!(parsed.context_path, "/api");
    }
}
