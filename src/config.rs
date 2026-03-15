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
    pub socket_path: Option<String>,
}

fn default_idle_timeout_secs() -> u64 {
    3600
}
fn default_debounce_ms() -> u64 {
    200
}
fn default_format() -> String {
    "{change_id} {bookmarks}{metrics} {state}".to_string()
}
fn default_bookmark_search_depth() -> u32 {
    10
}

impl Default for Config {
    fn default() -> Self {
        Self {
            idle_timeout_secs: default_idle_timeout_secs(),
            debounce_ms: default_debounce_ms(),
            format: default_format(),
            bookmark_search_depth: default_bookmark_search_depth(),
            socket_path: None,
        }
    }
}

impl Config {
    pub fn socket_path(&self) -> PathBuf {
        if let Some(ref path) = self.socket_path {
            return PathBuf::from(path);
        }
        let user = std::env::var("USER").unwrap_or_else(|_| "unknown".to_string());
        PathBuf::from(format!("/tmp/jj-status-daemon-{user}.sock"))
    }
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
        assert!(config.format.contains("{change_id}"));
        assert!(config.socket_path.is_none());
    }

    #[test]
    fn test_socket_path_default() {
        let config = Config::default();
        let path = config.socket_path();
        assert!(path.to_string_lossy().contains("jj-status-daemon"));
        assert!(path.to_string_lossy().ends_with(".sock"));
    }

    #[test]
    fn test_socket_path_custom() {
        let config = Config {
            socket_path: Some("/custom/path.sock".to_string()),
            ..Default::default()
        };
        assert_eq!(config.socket_path(), PathBuf::from("/custom/path.sock"));
    }

    #[test]
    fn test_config_from_toml() {
        let toml_str = r#"
idle_timeout_secs = 7200
debounce_ms = 500
format = "{change_id}"
bookmark_search_depth = 5
socket_path = "/tmp/test.sock"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.idle_timeout_secs, 7200);
        assert_eq!(config.debounce_ms, 500);
        assert_eq!(config.format, "{change_id}");
        assert_eq!(config.bookmark_search_depth, 5);
        assert_eq!(
            config.socket_path,
            Some("/tmp/test.sock".to_string())
        );
    }

    #[test]
    fn test_load_config_missing_file() {
        let config = load_config().unwrap();
        assert_eq!(config.idle_timeout_secs, 3600);
    }
}
