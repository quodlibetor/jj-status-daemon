use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::protocol::VcsKind;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_idle_timeout_secs")]
    pub idle_timeout_secs: u64,
    #[serde(default = "default_debounce_ms")]
    pub debounce_ms: u64,
    /// Explicit format template. If set, overrides `template_name`.
    #[serde(default)]
    pub format: Option<String>,
    /// Name of a built-in or user-defined template (default: "ascii").
    #[serde(default = "default_template_name")]
    pub template_name: String,
    /// User-defined named templates.
    #[serde(default)]
    pub templates: HashMap<String, String>,
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
fn default_template_name() -> String {
    "ascii".to_string()
}
fn default_bookmark_search_depth() -> u32 {
    10
}
fn default_color() -> bool {
    true
}

impl Config {
    /// Resolve the effective format template string.
    ///
    /// Priority: `format` field > user `templates[template_name]` > built-in template > ascii fallback.
    pub fn resolved_format(&self) -> String {
        tracing::debug!(template_name = %self.template_name, has_format = self.format.is_some(), "resolving format template");
        if let Some(fmt) = &self.format {
            return fmt.clone();
        }
        if let Some(user_tmpl) = self.templates.get(&self.template_name) {
            return user_tmpl.clone();
        }
        if let Some(builtin) = crate::jj::builtin_template(&self.template_name) {
            return builtin.to_string();
        }
        // Unknown template_name — fall back to ascii
        crate::jj::ASCII_FORMAT.to_string()
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            idle_timeout_secs: default_idle_timeout_secs(),
            debounce_ms: default_debounce_ms(),
            format: None,
            template_name: default_template_name(),
            templates: HashMap::new(),
            bookmark_search_depth: default_bookmark_search_depth(),
            color: default_color(),
        }
    }
}

/// Resolve the daemon runtime directory.
///
/// Checks `VCS_STATUS_DAEMON_DIR` env var first, then falls back
/// to `/tmp/vcs-status-daemon-$USER/`.
///
/// Layout:
///   `<dir>/sock`   — Unix domain socket
///   `<dir>/cache/` — cached status files
pub fn runtime_dir() -> PathBuf {
    if let Ok(path) = std::env::var("VCS_STATUS_DAEMON_DIR")
        && !path.is_empty()
    {
        return PathBuf::from(path);
    }
    let user = std::env::var("USER").unwrap_or_else(|_| "unknown".to_string());
    PathBuf::from(format!("/tmp/vcs-status-daemon-{user}"))
}

pub fn socket_path() -> PathBuf {
    runtime_dir().join("sock")
}

pub fn pid_path() -> PathBuf {
    runtime_dir().join("pid")
}

/// Find the repo root and VCS kind. jj wins if both `.jj/` and `.git/` are present.
pub fn find_repo_root(start: &Path) -> Option<(PathBuf, VcsKind)> {
    let mut dir = start.to_path_buf();
    loop {
        if dir.join(".jj").is_dir() {
            return Some((dir, VcsKind::Jj));
        }
        if dir.join(".git").exists() {
            return Some((dir, VcsKind::Git));
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// The cache directory: `<runtime_dir>/cache/`
pub fn cache_dir() -> PathBuf {
    runtime_dir().join("cache")
}

/// Encode a path as a flat filename: `/Users/bwm/repos/foo` → `%Users%bwm%repos%foo`
fn path_to_cache_name(path: &Path) -> String {
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    canonical.to_string_lossy().replace('/', "%")
}

/// Cache file path for a directory (repo root or any queried subdirectory).
pub fn cache_file_path(dir: &Path) -> PathBuf {
    cache_dir().join(path_to_cache_name(dir))
}

pub fn config_path() -> Option<PathBuf> {
    // Check XDG-style ~/.config first (cross-platform, and what most CLI tools use on macOS),
    // then fall back to the platform-native config dir (~/Library/Application Support on macOS).
    if let Some(home) = dirs::home_dir() {
        let xdg_path = home
            .join(".config")
            .join("vcs-status-daemon")
            .join("config.toml");
        if xdg_path.exists() {
            return Some(xdg_path);
        }
    }
    dirs::config_dir().map(|d| d.join("vcs-status-daemon").join("config.toml"))
}

/// The path where `config init` will write. Prefers ~/.config/ on all platforms.
pub fn config_init_path() -> Result<PathBuf> {
    let home =
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("could not determine home directory"))?;
    Ok(home
        .join(".config")
        .join("vcs-status-daemon")
        .join("config.toml"))
}

pub const DEFAULT_CONFIG_TOML: &str = r##"# vcs-status-daemon configuration
# See https://github.com/quodlibetor/vcs-status-daemon for full documentation.

# How long (in seconds) the daemon stays alive without any queries.
# idle_timeout_secs = 3600

# Debounce interval (in milliseconds) for filesystem change events.
# Lower values make the cache update faster; higher values reduce CPU usage.
# debounce_ms = 200

# How many ancestors of @ to search for bookmarks (jj only).
# bookmark_search_depth = 10

# Whether to include ANSI color codes in the output.
# Set to false if your shell prompt handles colors separately.
# color = true

# --------------------------------------------------------------------------
# Templates
# --------------------------------------------------------------------------
# The status output is rendered using the Tera template engine (Jinja2-like).
#
# Built-in templates: "ascii" (default), "nerdfont"
# Select one with template_name, or define your own below.

# Which template to use. Built-in options: "ascii", "nerdfont"
# template_name = "ascii"

# Override template_name with an inline format string.
# If set, this takes priority over template_name and user-defined templates.
# format = "{{ change_id }} {{ branch }}"

# --------------------------------------------------------------------------
# Available template variables
# --------------------------------------------------------------------------
# VCS type:
#   is_jj, is_git                  — booleans
#
# Shared:
#   commit_id                      — short commit hash
#   description                    — commit message summary (first line)
#   empty                          — true if the working copy has no changes
#   conflict                       — true if there are conflicts
#
# Diff stats (unstaged = working tree vs index):
#   files_changed, lines_added, lines_removed
#
# Diff stats (staged = index vs HEAD, git only, always 0 for jj):
#   staged_files_changed, staged_lines_added, staged_lines_removed
#
# Diff stats (total = working tree vs HEAD):
#   total_files_changed, total_lines_added, total_lines_removed
#
# jj-only:
#   change_id                      — jj change ID (short)
#   bookmarks                      — list of { name, distance, display }
#   divergent, hidden, immutable   — booleans
#
# git-only:
#   branch                         — current branch name
#
# Color codes (when color = true):
#   RED, GREEN, YELLOW, BLUE, MAGENTA, CYAN, WHITE
#   BRIGHT_RED, BRIGHT_GREEN, BRIGHT_YELLOW, BRIGHT_BLUE,
#   BRIGHT_MAGENTA, BRIGHT_CYAN, BRIGHT_WHITE
#   BOLD, RST (reset)
# --------------------------------------------------------------------------

# User-defined templates. Reference by name via template_name.
# [templates]
# minimal = "{{ change_id }}"
# my_custom = """\
# {% if is_jj %}{{ change_id }}{% elif is_git %}{{ branch }}{% endif %}\
# {% if empty %} (empty){% endif %}\
# """
"##;

pub fn load_config() -> Result<Config> {
    let Some(path) = config_path() else {
        tracing::debug!("no config dir found, using defaults");
        return Ok(Config::default());
    };
    if !path.exists() {
        tracing::debug!(path = %path.display(), "config file not found, using defaults");
        return Ok(Config::default());
    }
    let contents = std::fs::read_to_string(&path)?;
    match toml::from_str::<Config>(&contents) {
        Ok(config) => {
            tracing::info!(
                path = %path.display(),
                template_name = %config.template_name,
                has_format = config.format.is_some(),
                "loaded config"
            );
            Ok(config)
        }
        Err(e) => {
            tracing::error!(path = %path.display(), error = %e, "failed to parse config, using defaults");
            Ok(Config::default())
        }
    }
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
        assert_eq!(config.template_name, "ascii");
        assert!(config.format.is_none());
        assert!(config.resolved_format().contains("change_id"));
    }

    #[test]
    fn test_config_format_overrides_template_name() {
        let toml_str = r#"
template_name = "nerdfont"
format = "{{ change_id }}"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        // Explicit format wins over template_name
        assert_eq!(config.resolved_format(), "{{ change_id }}");
    }

    #[test]
    fn test_config_template_name_nerdfont() {
        let toml_str = r#"
template_name = "nerdfont"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert!(config.resolved_format().contains("󱗆"));
    }

    #[test]
    fn test_config_user_template() {
        let toml_str = r#"
template_name = "minimal"

[templates]
minimal = "{{ commit_id }}"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.resolved_format(), "{{ commit_id }}");
    }

    #[test]
    fn test_config_user_template_overrides_builtin() {
        let toml_str = r#"
template_name = "ascii"

[templates]
ascii = "custom ascii: {{ commit_id }}"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.resolved_format(), "custom ascii: {{ commit_id }}");
    }

    #[test]
    fn test_config_unknown_template_falls_back() {
        let toml_str = r#"
template_name = "nonexistent"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        // Should fall back to ascii
        assert!(config.resolved_format().contains("change_id"));
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
        assert_eq!(config.format, Some("{{ change_id }}".to_string()));
        assert_eq!(config.bookmark_search_depth, 5);
    }

    #[test]
    fn test_load_config_missing_file() {
        let config = load_config().unwrap();
        assert_eq!(config.idle_timeout_secs, 3600);
    }
}
