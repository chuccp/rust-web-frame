//! A directory of files that are expensive to produce and cheap to keep.
//!
//! The shape is "generate once, serve forever": a key made of the values that identify the
//! artefact — a user id, a report date, a template version — maps to a path, and the file
//! behind it is produced only if it is not already there.
//!
//! ```no_run
//! # use rust_web_frame::component::LocalCache;
//! # use rust_web_frame::web::{FileResponse, Result};
//! # async fn demo(cache: &LocalCache) -> Result<FileResponse> {
//! // Renders `report-2026-10.pdf` once, and serves the file from then on.
//! let file = cache
//!     .file_of(["report", "2026-10"], "pdf", || async {
//!         Ok(render_the_report().await)
//!     })
//!     .await?;
//! # Ok(file)
//! # }
//! # async fn render_the_report() -> Vec<u8> { Vec::new() }
//! ```
//!
//! Writes go to a temporary file and are renamed into place, so a reader — or a second
//! process — never sees a half-written artefact. That matters more than it sounds: without
//! it, two concurrent requests for the same missing file can both start generating it and
//! one of them can serve a truncated result.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::config::Config;
use crate::core::{Context, Service};
use crate::web::{Error, FileResponse, Result};

/// Config section that holds the local cache settings.
pub const LOCAL_CACHE_CONFIG_KEY: &str = "local_cache";

/// Local cache configuration, read from the `local_cache` section.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct LocalCacheConfig {
    /// Where the files live.
    pub path: PathBuf,
    /// A prefix added to every key, so two applications can share a directory.
    pub namespace: String,
}

impl Default for LocalCacheConfig {
    fn default() -> Self {
        Self {
            path: PathBuf::from("./local_cache"),
            namespace: String::new(),
        }
    }
}

/// A directory of artefacts, keyed by the values that identify them.
pub struct LocalCache {
    config: LocalCacheConfig,
    /// Serialises generation, so two requests for the same missing file produce it once.
    generating: Mutex<()>,
}

impl std::fmt::Debug for LocalCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalCache")
            .field("path", &self.config.path)
            .field("namespace", &self.config.namespace)
            .finish()
    }
}

impl Default for LocalCache {
    fn default() -> Self {
        Self::new(LocalCacheConfig::default())
    }
}

impl LocalCache {
    /// A cache rooted at `config.path`.
    #[must_use]
    pub fn new(config: LocalCacheConfig) -> Self {
        Self {
            config,
            generating: Mutex::new(()),
        }
    }

    /// A cache rooted where the `local_cache` section says.
    ///
    /// Registering a component is what puts it in the container, so this is how a cache is
    /// built in an application that configures one:
    ///
    /// ```no_run
    /// # use rust_web_frame::Builder;
    /// # use rust_web_frame::component::LocalCache;
    /// # use rust_web_frame::config::Config;
    /// let config = Config::empty();
    /// let mut builder = Builder::new(config.clone());
    /// builder.service(LocalCache::from_config(&config));
    /// ```
    #[must_use]
    pub fn from_config(config: &Config) -> Self {
        Self::new(config.section(LOCAL_CACHE_CONFIG_KEY).unwrap_or_default())
    }

    /// The configured root.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.config.path
    }

    /// The relative path a key maps to, e.g. `report/2026-10.pdf`.
    ///
    /// Each value becomes one path segment, sanitised: a value that contains a separator
    /// would otherwise let a caller write outside the cache, which is a directory of
    /// files the application serves.
    #[must_use]
    pub fn key_path<I, S>(&self, values: I, suffix: &str) -> PathBuf
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut path = PathBuf::new();
        if let Some(namespace) = sanitise(&self.config.namespace) {
            path.push(namespace);
        }

        let segments: Vec<String> = values
            .into_iter()
            .filter_map(|value| sanitise(value.as_ref()))
            .collect();

        // Everything but the last value is a directory; the last one names the file, so a
        // key of `["report", "2026-10"]` with suffix `pdf` is `report/2026-10.pdf`.
        if let Some((last, rest)) = segments.split_last() {
            for segment in rest {
                path.push(segment);
            }
            let suffix = suffix.trim_start_matches('.');
            path.push(if suffix.is_empty() {
                last.clone()
            } else {
                format!("{last}.{suffix}")
            });
        } else if !suffix.is_empty() {
            path.push(format!("file.{}", suffix.trim_start_matches('.')));
        }

        path
    }

    /// The full path a key maps to.
    #[must_use]
    pub fn full_path<I, S>(&self, values: I, suffix: &str) -> PathBuf
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.config.path.join(self.key_path(values, suffix))
    }

    /// Reports whether the artefact for a key already exists.
    #[must_use]
    pub fn has<I, S>(&self, values: I, suffix: &str) -> bool
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.full_path(values, suffix).is_file()
    }

    /// Removes an artefact, reporting whether there was one.
    ///
    /// # Errors
    ///
    /// Fails when the file exists but cannot be removed.
    pub async fn invalidate<I, S>(&self, values: I, suffix: &str) -> Result<bool>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let path = self.full_path(values, suffix);
        match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(true),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(err) => Err(Error::from(err)),
        }
    }

    /// The artefact for a key, generating it when it is missing.
    ///
    /// `generate` is called at most once per missing artefact, even when several requests
    /// ask for it at the same moment.
    ///
    /// # Errors
    ///
    /// Whatever `generate` returns, or a filesystem failure.
    pub async fn file_of<I, S, F, Fut>(
        &self,
        values: I,
        suffix: &str,
        generate: F,
    ) -> Result<FileResponse>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<Vec<u8>>>,
    {
        let path = self.full_path(values, suffix);
        if path.is_file() {
            return Ok(FileResponse::new(path));
        }

        // One generator at a time. A tighter design would key the lock by path, but that
        // needs a map of locks that has to be cleaned up; a single lock costs nothing at
        // the scale this component is for, and it is correct.
        let _guard = self.generating.lock().await;
        // Someone else may have produced it while this task waited for the lock.
        if path.is_file() {
            return Ok(FileResponse::new(path));
        }

        let bytes = generate().await?;
        self.write(&path, &bytes).await?;
        Ok(FileResponse::new(path))
    }

    /// Writes bytes to a key, replacing whatever was there.
    ///
    /// # Errors
    ///
    /// Fails when the directory cannot be created or the file cannot be written.
    pub async fn write(&self, path: &Path, bytes: &[u8]) -> Result<()> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        // Write beside the target and rename, so a reader never sees a partial file.
        let temporary = path.with_extension("part");
        tokio::fs::write(&temporary, bytes).await?;
        tokio::fs::rename(&temporary, path).await?;
        Ok(())
    }

    /// Writes bytes to the path a key maps to.
    ///
    /// # Errors
    ///
    /// Fails when the directory cannot be created or the file cannot be written.
    pub async fn put<I, S>(&self, values: I, suffix: &str, bytes: &[u8]) -> Result<PathBuf>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let path = self.full_path(values, suffix);
        self.write(&path, bytes).await?;
        Ok(path)
    }

    /// Writes a base64 string — what a browser sends for a pasted or cropped image — to a
    /// file, returning its path.
    ///
    /// A `data:` URL prefix is accepted and ignored.
    ///
    /// # Errors
    ///
    /// Fails when the payload is not base64, or the file cannot be written.
    pub async fn put_base64<I, S>(&self, values: I, suffix: &str, payload: &str) -> Result<PathBuf>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let encoded = match payload.split_once(",") {
            Some((prefix, rest)) if prefix.starts_with("data:") => rest,
            _ => payload,
        };

        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded.trim())
            .map_err(|err| {
                Error::with_source(400, err).detail("the payload is not valid base64")
            })?;

        let path = self.full_path(values, suffix);
        self.write(&path, &bytes).await?;
        Ok(path)
    }
}

#[async_trait]
impl Service for LocalCache {
    /// Creates the cache directory.
    ///
    /// The settings came from whoever built the cache — [`LocalCache::from_config`] reads
    /// the `local_cache` section — because the registered component is the one the
    /// container keeps: an `init` that built its own configured copy and registered *that*
    /// would leave `ctx.service::<LocalCache>()` pointing at one cache while the paths a
    /// handler got from it belonged to another.
    ///
    /// A section that disagrees with the registered settings is reported rather than
    /// obeyed, so `LocalCache::default()` behind a configured `local_cache` section is
    /// visible in the log instead of silently ignoring it.
    async fn init(&self, ctx: &Context) -> Result<()> {
        if let Ok(section) = ctx
            .config()
            .section::<LocalCacheConfig>(LOCAL_CACHE_CONFIG_KEY)
            && section.path != self.config.path
        {
            tracing::warn!(
                registered = %self.config.path.display(),
                configured = %section.path.display(),
                "the `local_cache` section is ignored: register the cache with \
                 `LocalCache::from_config(&config)`"
            );
        }

        tokio::fs::create_dir_all(self.path())
            .await
            .map_err(|err| {
                Error::with_source(500, err).detail(format!(
                    "cannot create the cache directory {}",
                    self.path().display()
                ))
            })?;

        tracing::debug!(path = %self.path().display(), "local cache ready");
        Ok(())
    }
}

/// Reduces a value to one safe path segment, or `None` when nothing usable is left.
///
/// Everything that could climb out of the directory goes: separators, control characters,
/// the characters Windows refuses, and leading or trailing dots. A value made of nothing
/// else — `..`, `.` — contributes no segment at all, which beats contributing a placeholder
/// that would become a real directory name.
fn sanitise(value: &str) -> Option<String> {
    let cleaned: String = value
        .trim()
        .chars()
        .filter(|c| {
            !c.is_control()
                && !matches!(
                    c,
                    '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\0'
                )
        })
        .collect();

    let cleaned = cleaned.trim_matches('.').trim().to_owned();
    if cleaned.is_empty() {
        return None;
    }

    // A name that names a device is not a file name on Windows.
    let stem = cleaned.split('.').next().unwrap_or("").to_ascii_uppercase();
    if matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL") {
        return Some(format!("_{cleaned}"));
    }
    Some(cleaned)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    fn cache(dir: &Path) -> LocalCache {
        LocalCache::new(LocalCacheConfig {
            path: dir.to_path_buf(),
            namespace: String::new(),
        })
    }

    #[test]
    fn a_key_becomes_a_path() {
        let dir = tempfile::tempdir().unwrap();
        let cache = cache(dir.path());

        assert_eq!(
            cache.key_path(["report", "2026-10"], "pdf"),
            PathBuf::from("report/2026-10.pdf")
        );
        assert_eq!(
            cache.key_path(["single"], "txt"),
            PathBuf::from("single.txt")
        );
        assert_eq!(cache.key_path(["plain"], ""), PathBuf::from("plain"));
    }

    #[test]
    fn a_key_cannot_climb_out_of_the_directory() {
        let dir = tempfile::tempdir().unwrap();
        let cache = cache(dir.path());

        // Separators are stripped, so a traversal attempt becomes a name.
        let path = cache.key_path(["..", "..", "etc", "passwd"], "txt");
        assert_eq!(path, PathBuf::from("etc/passwd.txt"));
        assert!(!path.starts_with(".."));

        let sneaky = cache.key_path(["../../etc/passwd"], "txt");
        assert_eq!(sneaky, PathBuf::from("etcpasswd.txt"));

        // A key of nothing usable still yields a path inside the cache.
        let empty = cache.key_path([".."], "txt");
        assert_eq!(empty, PathBuf::from("file.txt"));
        assert!(!empty.is_absolute());
    }

    #[test]
    fn the_namespace_is_a_segment_of_its_own() {
        let dir = tempfile::tempdir().unwrap();
        let cache = LocalCache::new(LocalCacheConfig {
            path: dir.path().to_path_buf(),
            namespace: String::from("app-one"),
        });
        assert_eq!(cache.key_path(["a"], "txt"), PathBuf::from("app-one/a.txt"));
    }

    #[tokio::test]
    async fn an_artefact_is_generated_once_and_then_served() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let dir = tempfile::tempdir().unwrap();
        let cache = cache(dir.path());
        let runs = Arc::new(AtomicUsize::new(0));

        for _ in 0..3 {
            let runs = Arc::clone(&runs);
            let file = cache
                .file_of(["report", "2026-10"], "txt", move || async move {
                    runs.fetch_add(1, Ordering::SeqCst);
                    Ok(b"generated".to_vec())
                })
                .await
                .unwrap();
            assert_eq!(tokio::fs::read(file.path()).await.unwrap(), b"generated");
        }

        assert_eq!(
            runs.load(Ordering::SeqCst),
            1,
            "the second and third calls must reuse the file"
        );
    }

    #[tokio::test]
    async fn concurrent_requests_generate_it_once() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let dir = tempfile::tempdir().unwrap();
        let cache = Arc::new(cache(dir.path()));
        let runs = Arc::new(AtomicUsize::new(0));

        let mut tasks = Vec::new();
        for _ in 0..16 {
            let cache = Arc::clone(&cache);
            let runs = Arc::clone(&runs);
            tasks.push(tokio::spawn(async move {
                cache
                    .file_of(["hot"], "bin", move || async move {
                        runs.fetch_add(1, Ordering::SeqCst);
                        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                        Ok(vec![7u8; 1024])
                    })
                    .await
                    .unwrap()
            }));
        }

        for task in tasks {
            let file = task.await.unwrap();
            // Nobody served a half-written file: every reader saw all 1024 bytes.
            assert_eq!(tokio::fs::read(file.path()).await.unwrap().len(), 1024);
        }
        assert_eq!(runs.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_failed_generation_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let cache = cache(dir.path());

        let failed = cache
            .file_of(["broken"], "txt", || async {
                Err::<Vec<u8>, _>(Error::internal().detail("could not render"))
            })
            .await;
        assert!(failed.is_err());
        assert!(
            !cache.has(["broken"], "txt"),
            "a failure must not leave an artefact"
        );
    }

    #[tokio::test]
    async fn a_base64_payload_becomes_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let cache = cache(dir.path());

        // "hello" in base64, with the data-URL prefix a browser sends.
        let path = cache
            .put_base64(
                ["uploads", "note"],
                "txt",
                "data:text/plain;base64,aGVsbG8=",
            )
            .await
            .unwrap();
        assert_eq!(tokio::fs::read(&path).await.unwrap(), b"hello");

        let bare = cache
            .put_base64(["uploads", "bare"], "txt", "aGVsbG8=")
            .await
            .unwrap();
        assert_eq!(tokio::fs::read(&bare).await.unwrap(), b"hello");

        let broken = cache
            .put_base64(["uploads", "bad"], "txt", "not base64!!")
            .await;
        assert!(broken.is_err());
    }

    #[tokio::test]
    async fn invalidating_reports_whether_there_was_anything() {
        let dir = tempfile::tempdir().unwrap();
        let cache = cache(dir.path());

        cache.put(["a"], "txt", b"x").await.unwrap();
        assert!(cache.has(["a"], "txt"));
        assert!(cache.invalidate(["a"], "txt").await.unwrap());
        assert!(!cache.has(["a"], "txt"));
        assert!(!cache.invalidate(["a"], "txt").await.unwrap());
    }

    #[tokio::test]
    async fn the_directory_is_created_at_startup() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Context::empty();
        let service = LocalCache::new(LocalCacheConfig {
            path: dir.path().join("nested/does/not/exist"),
            namespace: String::new(),
        });

        service.init(&ctx).await.unwrap();

        assert!(
            service.path().is_dir(),
            "the directory is created at startup"
        );
    }
}
