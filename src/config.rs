use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_idle_timeout_secs")]
    pub idle_timeout_secs: u64,
    #[serde(default = "default_debounce_ms")]
    pub debounce_ms: u64,
    #[serde(default = "default_format")]
    pub format: String,
    #[serde(default = "default_bookmark_search_depth")]
    pub bookmark_search_depth: u32,
    #[serde(default = "default_color")]
    pub color: bool,
}

fn default_idle_timeout_secs() -> u64 {
    3600
}
fn default_debounce_ms() -> u64 {
    200
}
fn default_format() -> String {
    crate::jj::DEFAULT_FORMAT.to_string()
}
fn default_bookmark_search_depth() -> u32 {
    10
}
fn default_color() -> bool {
    true
}

impl Default for Config {
    fn default() -> Self {
        Self {
            idle_timeout_secs: default_idle_timeout_secs(),
            debounce_ms: default_debounce_ms(),
            format: default_format(),
            bookmark_search_depth: default_bookmark_search_depth(),
            color: default_color(),
        }
    }
}

/// Resolve the daemon socket path.
///
/// Checks `JJ_STATUS_DAEMON_SOCKET_PATH` env var first, then falls back
/// to `/tmp/jj-status-daemon-$USER.sock`.
pub fn socket_path() -> PathBuf {
    if let Ok(path) = std::env::var("JJ_STATUS_DAEMON_SOCKET_PATH") {
        if !path.is_empty() {
            return PathBuf::from(path);
        }
    }
    let user = std::env::var("USER").unwrap_or_else(|_| "unknown".to_string());
    PathBuf::from(format!("/tmp/jj-status-daemon-{user}.sock"))
}

pub fn config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("jj-status-daemon").join("config.toml"))
}

pub fn load_config() -> Result<Config> {
    let Some(path) = config_path() else {
        return Ok(Config::default());
    };
    if !path.exists() {
        return Ok(Config::default());
    }
    let contents = std::fs::read_to_string(&path)?;
    let config: Config = toml::from_str(&contents)?;
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = Config::default();
        assert_eq!(config.idle_timeout_secs, 3600);
        assert_eq!(config.debounce_ms, 200);
        assert_eq!(config.bookmark_search_depth, 10);
        assert!(config.format.contains("change_id"));
    }

    #[test]
    fn test_config_from_toml() {
        let toml_str = r#"
idle_timeout_secs = 7200
debounce_ms = 500
format = "{{ change_id }}"
bookmark_search_depth = 5
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.idle_timeout_secs, 7200);
        assert_eq!(config.debounce_ms, 500);
        assert_eq!(config.format, "{{ change_id }}");
        assert_eq!(config.bookmark_search_depth, 5);
    }

    #[test]
    fn test_load_config_missing_file() {
        let config = load_config().unwrap();
        assert_eq!(config.idle_timeout_secs, 3600);
    }
}
