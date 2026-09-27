//! Service configuration.
//!
//! Settings use the same JSON layout as the reference xBrowserSync API
//! (`config/settings.json`), so an existing settings file can be reused as-is. Values are
//! layered, later layers winning:
//!
//! 1. built-in defaults (`config/settings.default.json`, embedded in the binary);
//! 2. the JSON object in the `MARKSYNC_SETTINGS_JSON` environment variable, meant for
//!    deployment-wide defaults such as those baked into the Docker image;
//! 3. the settings file (`--config <path>`, `MARKSYNC_CONFIG`, or `config/settings.json`).
//!
//! Objects are merged recursively and arrays are concatenated, matching `deepmerge`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::Value;

/// API version reported by `/info` and used when a request carries no `Accept-Version`.
/// Tracks the xBrowserSync API release whose contract this server implements.
pub const API_VERSION: &str = "1.1.13";

const DEFAULT_SETTINGS: &str = include_str!("../config/settings.default.json");
const DEFAULT_SETTINGS_PATH: &str = "config/settings.json";

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    pub allowed_origins: Vec<String>,
    pub daily_new_syncs_limit: u64,
    pub db: DbConfig,
    pub location: String,
    pub log: LogConfig,
    pub max_syncs: u64,
    pub max_sync_size: usize,
    pub server: ServerConfig,
    pub status: StatusConfig,
    /// Syncs not accessed for this many days are deleted (0 disables expiry).
    pub sync_expiry_days: u64,
    pub throttle: ThrottleConfig,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DbType {
    Sqlite,
    #[serde(alias = "mongo")]
    Mongodb,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DbConfig {
    #[serde(rename = "type")]
    pub kind: DbType,
    /// SQLite database file.
    pub path: String,
    /// Full MongoDB connection string; when set, the host/port/credential fields are ignored.
    pub uri: String,
    pub auth_source: String,
    pub conn_timeout: u64,
    pub host: String,
    pub name: String,
    pub password: String,
    pub port: u16,
    pub ssl: bool,
    #[serde(rename = "useSRV")]
    pub use_srv: bool,
    pub username: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LogConfig {
    pub file: FileLogConfig,
    pub stdout: StdoutLogConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileLogConfig {
    pub enabled: bool,
    pub level: String,
    pub path: String,
    pub rotated_files_to_keep: usize,
    pub rotation_period: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StdoutLogConfig {
    pub enabled: bool,
    /// `text` (human-readable) or `json` (one object per line).
    pub format: String,
    pub level: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerConfig {
    pub behind_proxy: bool,
    pub host: String,
    pub https: HttpsConfig,
    pub port: u16,
    pub relative_path: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HttpsConfig {
    pub cert_path: String,
    pub enabled: bool,
    pub key_path: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusConfig {
    pub allow_new_syncs: bool,
    pub message: String,
    pub online: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThrottleConfig {
    pub max_requests: u64,
    /// Throttle window in milliseconds.
    pub time_window: u64,
}

impl Config {
    /// Loads settings from the environment and the settings file.
    ///
    /// `explicit_path` (from `--config`) must exist; the default path is optional.
    pub fn load(explicit_path: Option<&Path>) -> Result<Self> {
        let path =
            explicit_path.map(Path::to_path_buf).or_else(|| std::env::var_os("MARKSYNC_CONFIG").map(PathBuf::from));
        let user = match &path {
            Some(path) => Some(read_json_file(path)?),
            None => {
                let default = Path::new(DEFAULT_SETTINGS_PATH);
                if default.exists() { Some(read_json_file(default)?) } else { None }
            }
        };
        let env = match std::env::var("MARKSYNC_SETTINGS_JSON") {
            Ok(raw) if !raw.trim().is_empty() => {
                Some(serde_json::from_str(&raw).context("MARKSYNC_SETTINGS_JSON is not valid JSON")?)
            }
            _ => None,
        };
        let mut config = Self::from_layers(env.into_iter().chain(user))?;
        config.apply_credential_env();
        Ok(config)
    }

    /// Builds a config by merging the given JSON layers over the defaults.
    pub fn from_layers(layers: impl IntoIterator<Item = Value>) -> Result<Self> {
        let mut merged: Value =
            serde_json::from_str(DEFAULT_SETTINGS).expect("embedded default settings are valid JSON");
        for layer in layers {
            if !layer.is_object() {
                bail!("settings must be a JSON object");
            }
            deep_merge(&mut merged, layer);
        }
        let config: Config = serde_json::from_value(merged).context("invalid settings")?;
        config.validate()?;
        Ok(config)
    }

    /// Default settings, optionally overridden by a single JSON layer (handy in tests).
    pub fn with_overrides(overrides: Value) -> Result<Self> {
        Self::from_layers([overrides])
    }

    fn apply_credential_env(&mut self) {
        // Same fallbacks as the reference API: settings take precedence over the environment.
        if self.db.username.is_empty() {
            if let Ok(user) = std::env::var("XBROWSERSYNC_DB_USER") {
                self.db.username = user;
            }
        }
        if self.db.password.is_empty() {
            if let Ok(pwd) = std::env::var("XBROWSERSYNC_DB_PWD") {
                self.db.password = pwd;
            }
        }
    }

    fn validate(&self) -> Result<()> {
        if !crate::location::is_valid_location_code(&self.location) {
            bail!("location {:?} is not a valid ISO 3166-1 alpha-2 country code", self.location);
        }
        if !self.server.relative_path.starts_with('/') {
            bail!("server.relativePath must start with '/'");
        }
        if !matches!(self.log.stdout.format.as_str(), "text" | "json") {
            bail!("log.stdout.format must be \"text\" or \"json\"");
        }
        if self.server.https.enabled
            && (self.server.https.cert_path.is_empty() || self.server.https.key_path.is_empty())
        {
            bail!("server.https.certPath and server.https.keyPath are required when https is enabled");
        }
        Ok(())
    }

    /// Relative path normalised to start and end with `/`.
    pub fn base_path(&self) -> String {
        let trimmed = self.server.relative_path.trim_end_matches('/');
        format!("{trimmed}/")
    }
}

fn read_json_file(path: &Path) -> Result<Value> {
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("unable to read settings file {}", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("settings file {} is not valid JSON", path.display()))
}

fn deep_merge(target: &mut Value, source: Value) {
    match (target, source) {
        (Value::Object(target), Value::Object(source)) => {
            for (key, value) in source {
                match target.get_mut(&key) {
                    Some(existing) => deep_merge(existing, value),
                    None => {
                        target.insert(key, value);
                    }
                }
            }
        }
        (Value::Array(target), Value::Array(source)) => target.extend(source),
        (target, source) => *target = source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn defaults_match_reference_api() {
        let config = Config::with_overrides(json!({})).unwrap();
        assert_eq!(config.max_sync_size, 512_000);
        assert_eq!(config.max_syncs, 5242);
        assert_eq!(config.daily_new_syncs_limit, 3);
        assert_eq!(config.throttle.max_requests, 1000);
        assert_eq!(config.throttle.time_window, 300_000);
        assert_eq!(config.server.port, 8080);
        assert_eq!(config.db.kind, DbType::Sqlite);
        assert!(config.status.online && config.status.allow_new_syncs);
    }

    #[test]
    fn merges_nested_objects_and_concatenates_arrays() {
        let config = Config::from_layers([
            json!({ "allowedOrigins": ["a"], "server": { "port": 9000 } }),
            json!({ "allowedOrigins": ["b"], "db": { "type": "mongodb" } }),
        ])
        .unwrap();
        assert_eq!(config.allowed_origins, ["a", "b"]);
        assert_eq!(config.server.port, 9000);
        assert_eq!(config.server.host, "127.0.0.1");
        assert_eq!(config.db.kind, DbType::Mongodb);
    }

    #[test]
    fn rejects_invalid_location() {
        assert!(Config::with_overrides(json!({ "location": "XX" })).is_err());
        assert!(Config::with_overrides(json!({ "location": "gb" })).is_ok());
    }

    #[test]
    fn ignores_unknown_keys_from_reference_settings() {
        let config = Config::with_overrides(json!({ "tests": { "db": "x", "port": 1 }, "version": "9.9.9" }));
        assert!(config.is_ok());
    }

    #[test]
    fn base_path_is_normalised() {
        let config = Config::with_overrides(json!({ "server": { "relativePath": "/api" } })).unwrap();
        assert_eq!(config.base_path(), "/api/");
        let config = Config::with_overrides(json!({})).unwrap();
        assert_eq!(config.base_path(), "/");
    }
}
