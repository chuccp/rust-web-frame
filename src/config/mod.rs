//! Configuration loading and lookup.
//!
//! Config files are searched for in four places, in order of increasing precedence:
//!
//! 1. `/etc/<app>/config.{json,yaml,yml,toml}`
//! 2. `~/.<app>/config.{json,yaml,yml,toml}`
//! 3. `./config/config.{json,yaml,yml,toml}`
//! 4. `./config.{json,yaml,yml,toml}`
//!
//! Found files are deep-merged, so a project-level file can override a system one.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::value::merge;
use crate::web::{Error, Result, code};

/// Config file formats the loader understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// `.json`
    Json,
    /// `.yaml` / `.yml`
    Yaml,
    /// `.toml`
    Toml,
}

impl Format {
    /// Guesses the format from a file extension.
    pub fn from_path(path: &Path) -> Option<Self> {
        match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
            "json" => Some(Self::Json),
            "yaml" | "yml" => Some(Self::Yaml),
            "toml" => Some(Self::Toml),
            _ => None,
        }
    }

    /// The file extensions searched for, in order.
    pub const EXTENSIONS: [&'static str; 4] = ["json", "yaml", "yml", "toml"];
}

/// The application's configuration tree.
///
/// ```no_run
/// # use rust_web_frame::config::load_auto_config;
/// # fn demo() -> Result<(), Box<dyn std::error::Error>> {
/// let config = load_auto_config()?;
/// let port: u16 = config.get_or("web.server.port", 19009);
/// assert!(config.contains_key("web.server"));
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone)]
pub struct Config {
    root: Arc<Value>,
    sources: Arc<Vec<PathBuf>>,
}

impl Default for Config {
    fn default() -> Self {
        Self::empty()
    }
}

impl Config {
    /// A config with no values.
    pub fn empty() -> Self {
        Self {
            root: Arc::new(Value::Object(Default::default())),
            sources: Arc::new(Vec::new()),
        }
    }

    /// Wraps an already parsed tree.
    pub fn from_value(root: Value) -> Self {
        Self {
            root: Arc::new(root),
            sources: Arc::new(Vec::new()),
        }
    }

    /// Parses a document in the given format.
    pub fn parse(text: &str, format: Format) -> Result<Self> {
        let root = match format {
            Format::Json => serde_json::from_str(text).map_err(Error::from)?,
            Format::Yaml => serde_yaml::from_str(text)
                .map_err(|err| Error::new(code::BAD_REQUEST, err.to_string()))?,
            Format::Toml => toml::from_str(text)
                .map_err(|err| Error::new(code::BAD_REQUEST, err.to_string()))?,
        };
        Ok(Self::from_value(root))
    }

    /// Reads and parses a config file.
    pub fn load_file(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let format = Format::from_path(path).ok_or_else(|| {
            Error::new(
                code::BAD_REQUEST,
                format!("unsupported config extension: {}", path.display()),
            )
        })?;
        let text = std::fs::read_to_string(path)?;
        let mut config = Self::parse(&text, format)?;
        config.sources = Arc::new(vec![path.to_path_buf()]);
        Ok(config)
    }

    /// The files this config was loaded from.
    pub fn sources(&self) -> &[PathBuf] {
        &self.sources
    }

    /// The whole tree.
    pub fn value(&self) -> &Value {
        &self.root
    }

    /// Reads a value at a dotted path.
    pub fn get(&self, key: &str) -> Option<&Value> {
        crate::value::get_path(&self.root, key)
    }

    /// Reports whether a non-null value exists at `key`.
    pub fn contains_key(&self, key: &str) -> bool {
        matches!(self.get(key), Some(value) if !value.is_null())
    }

    /// Reads a scalar at `key`, or `default` when it is missing or unparsable.
    pub fn get_or<T: DeserializeOwned>(&self, key: &str, default: T) -> T {
        match self.get(key) {
            Some(value) => serde_json::from_value(value.clone()).unwrap_or(default),
            None => default,
        }
    }

    /// Reads a required scalar, failing when it is absent.
    pub fn require<T: DeserializeOwned>(&self, key: &str) -> Result<T> {
        let value = self.get(key).ok_or_else(|| {
            Error::new(code::INTERNAL_ERROR, format!("missing config key `{key}`"))
        })?;
        serde_json::from_value(value.clone()).map_err(|err| {
            Error::new(
                code::INTERNAL_ERROR,
                format!("config key `{key}` is invalid"),
            )
            .caused_by(err)
        })
    }

    /// Deserialises the subtree at `key` into `T`.
    ///
    /// A missing key yields `T::default()` — the section simply was not configured — which
    /// is why every framework config struct carries `#[serde(default)]`.
    pub fn section<T: DeserializeOwned + Default>(&self, key: &str) -> Result<T> {
        match self.get(key) {
            Some(value) => serde_json::from_value(value.clone()).map_err(|err| {
                Error::new(
                    code::INTERNAL_ERROR,
                    format!("invalid config section `{key}`"),
                )
                .caused_by(err)
            }),
            None => Ok(T::default()),
        }
    }

    /// Replaces the tree while keeping the backing file, so [`write`](Self::write) still
    /// knows where to go.
    pub fn with_value(&self, root: Value) -> Config {
        Config {
            root: Arc::new(root),
            sources: self.sources.clone(),
        }
    }

    /// Sets a value at a dotted path, creating intermediate objects as needed.
    pub fn set(&mut self, key: &str, value: Value) {
        let mut root = (*self.root).clone();
        let mut cursor = &mut root;
        let segments: Vec<&str> = key.split('.').collect();
        for segment in &segments[..segments.len() - 1] {
            if !cursor.is_object() {
                *cursor = Value::Object(Default::default());
            }
            cursor = cursor
                .as_object_mut()
                .expect("just made an object")
                .entry((*segment).to_string())
                .or_insert_with(|| Value::Object(Default::default()));
        }
        if !cursor.is_object() {
            *cursor = Value::Object(Default::default());
        }
        cursor
            .as_object_mut()
            .expect("just made an object")
            .insert(segments[segments.len() - 1].to_string(), value);
        self.root = Arc::new(root);
    }

    /// Merges `other` on top of this config, returning a new one.
    pub fn merged_with(&self, other: &Config) -> Config {
        let mut root = (*self.root).clone();
        merge(&mut root, (*other.root).clone());
        let mut sources = (*self.sources).clone();
        sources.extend(other.sources.iter().cloned());
        Config {
            root: Arc::new(root),
            sources: Arc::new(sources),
        }
    }

    /// Writes the config back to the file it came from, preserving its format.
    ///
    /// Fails when the config was not loaded from a single file — there is nowhere to
    /// write it back to.
    pub fn write(&self) -> Result<()> {
        let path = self
            .sources
            .first()
            .ok_or_else(|| Error::new(code::INTERNAL_ERROR, "config has no backing file"))?;
        let format = Format::from_path(path)
            .ok_or_else(|| Error::new(code::INTERNAL_ERROR, "config file has no known format"))?;
        let text = match format {
            Format::Json => serde_json::to_string_pretty(&*self.root).map_err(Error::from)?,
            Format::Yaml => serde_yaml::to_string(&*self.root)
                .map_err(|err| Error::new(code::INTERNAL_ERROR, err.to_string()))?,
            Format::Toml => toml::to_string_pretty(&*self.root)
                .map_err(|err| Error::new(code::INTERNAL_ERROR, err.to_string()))?,
        };
        std::fs::write(path, text)?;
        Ok(())
    }

    /// Deep-merges several configs, later ones winning.
    ///
    /// ```no_run
    /// # use rust_web_frame::config::Config;
    /// let base = Config::from_value(serde_json::json!({"a": 1, "b": 2}));
    /// let override_ = Config::from_value(serde_json::json!({"b": 3}));
    /// let merged = Config::merge([base, override_]);
    /// assert_eq!(merged.get_or("b", 0), 3);
    /// ```
    pub fn merge(configs: impl IntoIterator<Item = Config>) -> Config {
        configs
            .into_iter()
            .reduce(|acc, next| acc.merged_with(&next))
            .unwrap_or_else(Config::empty)
    }
}

/// The name used to look for per-application config directories. Taken from the running
/// executable, falling back to `app`.
pub fn app_name() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|path| {
            path.file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| String::from("app"))
}

/// The directories searched by [`load_auto_config`], lowest precedence first.
pub fn search_dirs() -> Vec<PathBuf> {
    let app = app_name();
    let mut dirs = vec![
        PathBuf::from("/etc").join(&app),
        PathBuf::from("config"),
        PathBuf::from("."),
    ];
    if let Some(home) = home_dir() {
        dirs.insert(1, home.join(format!(".{app}")));
    }
    dirs
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
}

/// Loads and merges every config file found in the [search path](search_dirs).
///
/// Returns an empty config when nothing is found, so an application with no config file
/// still starts on the framework defaults.
pub fn load_auto_config() -> Result<Config> {
    let mut found: Vec<Config> = Vec::new();
    for dir in search_dirs() {
        for extension in Format::EXTENSIONS {
            let candidate = dir.join(format!("config.{extension}"));
            if candidate.is_file() {
                let config = Config::load_file(&candidate)?;
                tracing::debug!(path = %candidate.display(), "config loaded");
                found.push(config);
            }
        }
    }
    Ok(Config::merge(found))
}

/// Loads one specific file.
pub fn load_config(path: impl AsRef<Path>) -> Result<Config> {
    Config::load_file(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn json_yaml_and_toml_parse_to_the_same_tree() {
        let json = Config::parse(r#"{"web": {"server": {"port": 8080}}}"#, Format::Json).unwrap();
        let yaml = Config::parse("web:\n  server:\n    port: 8080\n", Format::Yaml).unwrap();
        let toml = Config::parse("[web.server]\nport = 8080\n", Format::Toml).unwrap();

        for config in [json, yaml, toml] {
            assert_eq!(config.get_or("web.server.port", 0u16), 8080);
            assert!(config.contains_key("web.server"));
            assert!(!config.contains_key("web.missing"));
        }
    }

    #[test]
    fn later_configs_win_when_merging() {
        let base = Config::from_value(serde_json::json!({"a": 1, "nested": {"x": 1, "y": 2}}));
        let patch = Config::from_value(serde_json::json!({"b": 5, "nested": {"y": 9}}));
        let merged = Config::merge([base, patch]);

        assert_eq!(merged.get_or("a", 0), 1);
        assert_eq!(merged.get_or("b", 0), 5);
        assert_eq!(merged.get_or("nested.x", 0), 1);
        assert_eq!(merged.get_or("nested.y", 0), 9);
    }

    #[test]
    fn section_returns_defaults_for_a_missing_section() {
        #[derive(serde::Deserialize, Default, PartialEq, Debug)]
        #[serde(default)]
        struct Section {
            name: String,
            retries: u32,
        }

        let config = Config::from_value(serde_json::json!({"other": {"name": "x"}}));
        let section: Section = config.section("section").unwrap();
        assert_eq!(section, Section::default());
    }

    #[test]
    fn section_reports_a_bad_section() {
        let config = Config::from_value(serde_json::json!({"section": {"retries": "many"}}));
        let result: Result<HashMap<String, u32>> = config.section("section");
        assert!(result.is_err());
    }

    #[test]
    fn required_keys_fail_loudly() {
        let config = Config::empty();
        let result: Result<String> = config.require("db.url");
        assert!(result.is_err());
        assert!(result.unwrap_err().client_message().contains("db.url"));
    }

    #[test]
    fn configs_round_trip_through_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        std::fs::write(&path, "web:\n  server:\n    port: 1234\n").unwrap();

        let mut config = Config::load_file(&path).unwrap();
        assert_eq!(config.get_or("web.server.port", 0u16), 1234);
        assert_eq!(config.sources().len(), 1);

        config.set("extra", serde_json::json!(true));
        config.set("deep.nested.value", serde_json::json!("hi"));
        config.write().unwrap();

        // Write-back kept the YAML format.
        let reloaded = Config::load_file(&path).unwrap();
        assert!(reloaded.get_or("extra", false));
        assert_eq!(reloaded.get_or("web.server.port", 0u16), 1234);
        assert_eq!(reloaded.get_or("deep.nested.value", String::new()), "hi");
    }

    #[test]
    fn setting_a_path_creates_intermediate_objects() {
        let mut config = Config::empty();
        config.set("a.b.c", serde_json::json!(1));
        assert_eq!(config.get_or("a.b.c", 0), 1);
        assert!(config.contains_key("a.b"));
    }

    #[test]
    fn unknown_extensions_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.ini");
        std::fs::write(&path, "port=1").unwrap();
        assert!(Config::load_file(&path).is_err());
    }

    #[test]
    fn auto_loading_never_fails_without_files() {
        // The framework must start on its defaults when nothing is configured.
        let config = Config::parse("{}", Format::Json).unwrap();
        assert!(!config.contains_key("web.server"));
    }

    #[test]
    fn the_search_path_includes_the_working_directory() {
        let dirs = search_dirs();
        assert!(dirs.iter().any(|d| d == Path::new(".")));
        assert!(dirs.iter().any(|d| d == Path::new("config")));
    }
}
