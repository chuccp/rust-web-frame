//! Server configuration.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Configuration key under which the server section lives.
pub const SERVER_CONFIG_KEY: &str = "web.server";

/// Default maximum request body size: 10 MiB.
pub const DEFAULT_MAX_BODY_SIZE: i64 = 10 << 20;

/// Default listen port.
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
    /// Configured as `page404`.
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
    /// Cross-origin policy. `None` sends no CORS headers at all, which is the safe
    /// default: a browser on another origin simply cannot read the response.
    pub cors: Option<CorsConfig>,
}

/// Cross-origin resource sharing, read from the `web.server.cors` section.
///
/// Configured, it is applied as a layer outside the router, so a preflight `OPTIONS` is
/// answered before routing — such a request matches no route, and would otherwise be a
/// `405` with no CORS headers on it.
///
/// ```yaml
/// web:
///   server:
///     cors:
///       allowedOrigins: ["https://app.example.com"]
///       allowedMethods: [GET, POST]
///       allowedHeaders: [content-type, authorization]
///       allowCredentials: true
///       maxAge: 3600
/// ```
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct CorsConfig {
    /// Origins allowed to read responses. `["*"]` allows any, which browsers refuse to
    /// combine with credentials.
    pub allowed_origins: Vec<String>,
    /// Methods allowed on a preflight. Empty means the browser default set.
    pub allowed_methods: Vec<String>,
    /// Request headers a client may send. Empty means the browser default set.
    pub allowed_headers: Vec<String>,
    /// Response headers a client may read.
    pub exposed_headers: Vec<String>,
    /// Whether to send `Access-Control-Allow-Credentials`.
    pub allow_credentials: bool,
    /// How long a browser may cache the preflight, in seconds.
    pub max_age: Option<u64>,
}

impl CorsConfig {
    /// Reports whether this configuration would actually send anything.
    #[must_use]
    pub fn is_configured(&self) -> bool {
        !self.allowed_origins.is_empty()
            || !self.allowed_methods.is_empty()
            || !self.allowed_headers.is_empty()
    }
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
            cors: None,
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

    /// Normalises the route prefix in place: either empty, or `/something` with no
    /// trailing slash.
    ///
    /// A `contextPath` written as `api`, `/api/` or `/api` all mean the same thing, and
    /// only one of them joins correctly with a route path.
    pub fn normalise(&mut self) {
        let trimmed = self.context_path.trim().trim_matches('/').to_owned();
        self.context_path = if trimmed.is_empty() {
            String::new()
        } else {
            format!("/{trimmed}")
        };
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
    ///
    /// Clamped to what hyper can actually do: its http/1 reader asserts a minimum read
    /// buffer of 8 KiB, and passing less panics on the connection that happens to be
    /// serving the request. A ceiling below that is not expressible, so it is raised and
    /// said out loud rather than crashing a connection later.
    pub fn effective_max_header_bytes(&self) -> usize {
        const HYPER_MINIMUM: usize = 8 * 1024;

        let configured = if self.max_header_bytes > 0 {
            self.max_header_bytes
        } else {
            16 * 1024
        };

        if configured < HYPER_MINIMUM {
            tracing::warn!(
                configured,
                raisedTo = HYPER_MINIMUM,
                "`web.server.maxHeaderBytes` is below the 8 KiB a connection needs, raising it"
            );
            HYPER_MINIMUM
        } else {
            configured
        }
    }
}

/// TLS / HTTPS settings.
///
/// Two ways to get a certificate, and the first one configured wins:
///
/// * `certs` — PEM files on disk. They are re-read when they change, so a renewal written
///   by `certbot` takes effect without a restart.
/// * `hosts` — names to obtain and renew automatically over ACME (Let's Encrypt),
///   answering the challenge on TLS-ALPN-01 and caching the result in `cacheDir`.
///
/// With neither, the server serves a self-signed certificate, which is what a development
/// machine wants and what a browser will warn about.
///
/// ```yaml
/// web:
///   server:
///     ssl:
///       enabled: true
///       hosts: ["example.com", "www.example.com"]
///       # or, for certificates the operator already has:
///       certs:
///         - certFile: /etc/ssl/example.com.crt
///           keyFile: /etc/ssl/example.com.key
/// ```
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SslConfig {
    /// Whether HTTPS is enabled.
    pub enabled: bool,
    /// Domain names to obtain and renew automatically over ACME.
    pub hosts: Vec<String>,
    /// Local PEM certificate pairs. Takes precedence over `hosts`.
    pub certs: Vec<SslCertificate>,
    /// Where automatically obtained certificates are cached. Defaults to `./auto_cert`.
    pub cache_dir: Option<PathBuf>,
    /// Contact address sent to the ACME account. Some authorities require one.
    pub email: Option<String>,
}

/// One PEM certificate and its key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SslCertificate {
    /// PEM certificate chain.
    pub cert_file: PathBuf,
    /// PEM private key.
    pub key_file: PathBuf,
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
    fn a_tiny_header_cap_is_raised_to_what_hyper_can_do() {
        let mut config = ServerConfig::default();
        assert_eq!(config.effective_max_header_bytes(), 16 * 1024);

        // Below hyper's http/1 minimum, which would panic on the connection.
        config.max_header_bytes = 512;
        assert_eq!(config.effective_max_header_bytes(), 8 * 1024);

        config.max_header_bytes = 32 * 1024;
        assert_eq!(config.effective_max_header_bytes(), 32 * 1024);
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
    fn the_context_path_is_normalised() {
        for (written, expected) in [
            ("/api", "/api"),
            ("api", "/api"),
            ("/api/", "/api"),
            ("  /api/v1/  ", "/api/v1"),
            ("", ""),
            ("/", ""),
        ] {
            let mut config = ServerConfig {
                context_path: written.to_owned(),
                ..ServerConfig::default()
            };
            config.normalise();
            assert_eq!(config.context_path, expected, "from {written:?}");
        }
    }

    #[test]
    fn tls_takes_either_local_certificates_or_hosts() {
        let config: SslConfig = serde_json::from_value(serde_json::json!({
            "enabled": true,
            "hosts": ["example.com"],
            "certs": [{"certFile": "/a.crt", "keyFile": "/a.key"}],
            "cacheDir": "/tmp/certs",
            "email": "ops@example.com"
        }))
        .unwrap();

        assert!(config.enabled);
        assert_eq!(config.hosts, ["example.com"]);
        assert_eq!(
            config.certs[0].cert_file,
            std::path::PathBuf::from("/a.crt")
        );
        assert_eq!(
            config.cache_dir,
            Some(std::path::PathBuf::from("/tmp/certs"))
        );
        assert_eq!(config.email.as_deref(), Some("ops@example.com"));
    }

    #[test]
    fn cors_is_absent_unless_configured() {
        let config = ServerConfig::default();
        assert!(config.cors.is_none());

        let parsed: ServerConfig = serde_json::from_value(serde_json::json!({
            "cors": {"allowedOrigins": ["https://example.com"], "allowCredentials": true}
        }))
        .unwrap();
        let cors = parsed.cors.unwrap();
        assert_eq!(cors.allowed_origins, ["https://example.com"]);
        assert!(cors.allow_credentials);
        assert!(cors.is_configured());
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
