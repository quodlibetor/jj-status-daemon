use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::protocol::VcsKind;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default_idle_timeout_secs")]
    pub idle_timeout_secs: u64,
    /// Explicit format template. If set, overrides `template_name`.
    #[serde(default)]
    pub format: Option<String>,
    /// Name of a built-in or user-defined template (default: "ascii").
    #[serde(default = "default_template_name")]
    pub template_name: String,
    /// Explicit not-ready template. If set, overrides the built-in not-ready template.
    #[serde(default)]
    pub not_ready_format: Option<String>,
    /// User-defined named templates.
    #[serde(default)]
    pub templates: HashMap<String, String>,
    #[serde(default = "default_bookmark_search_depth")]
    pub bookmark_search_depth: u32,
    #[serde(default = "default_color")]
    pub color: bool,
    /// How long (ms) to wait for a fresh status before returning "not ready" or stale data.
    /// 0 means respond immediately.
    #[serde(default = "default_query_timeout_ms")]
    pub query_timeout_ms: u64,
}

fn default_idle_timeout_secs() -> u64 {
    3600
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
fn default_query_timeout_ms() -> u64 {
    150
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
        if let Some(builtin) = crate::template::builtin_template(&self.template_name) {
            return builtin.to_string();
        }
        // Unknown template_name — fall back to ascii
        crate::template::ASCII_FORMAT.to_string()
    }

    /// Resolve the not-ready template string.
    ///
    /// Priority: `not_ready_format` field > built-in not-ready template matching `template_name`.
    pub fn resolved_not_ready_format(&self) -> String {
        if let Some(fmt) = &self.not_ready_format {
            return fmt.clone();
        }
        crate::template::builtin_not_ready_template(&self.template_name).to_string()
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            idle_timeout_secs: default_idle_timeout_secs(),
            format: None,
            not_ready_format: None,
            template_name: default_template_name(),
            templates: HashMap::new(),
            bookmark_search_depth: default_bookmark_search_depth(),
            color: default_color(),
            query_timeout_ms: default_query_timeout_ms(),
        }
    }
}

/// Check that the current user is not root, unless allow_root is true
/// or VCS_STATUS_DAEMON_DIR is explicitly set.
pub fn check_not_root(allow_root: bool) -> Result<()> {
    if allow_root {
        return Ok(());
    }
    // If VCS_STATUS_DAEMON_DIR is explicitly set, the user knows what they're doing
    if let Ok(path) = std::env::var("VCS_STATUS_DAEMON_DIR")
        && !path.is_empty()
    {
        return Ok(());
    }
    let user = std::env::var("USER").unwrap_or_default();
    if user == "root" {
        anyhow::bail!("refusing to run as root (use --allow-root to override)");
    }
    Ok(())
}

/// Resolve the daemon runtime directory.
///
/// Checks `VCS_STATUS_DAEMON_DIR` env var first, then falls back
/// to `/tmp/vcs-status-daemon-$USER/`.
///
/// Layout:
///   `<dir>/sock`   — Unix domain socket
///   `<dir>/cache/` — cached status files
pub fn runtime_dir() -> Result<PathBuf> {
    if let Ok(path) = std::env::var("VCS_STATUS_DAEMON_DIR")
        && !path.is_empty()
    {
        return Ok(PathBuf::from(path));
    }
    let user = std::env::var("USER")
        .ok()
        .filter(|u| !u.is_empty())
        .ok_or_else(|| anyhow::anyhow!("$USER is not set; set $USER or $VCS_STATUS_DAEMON_DIR"))?;
    Ok(PathBuf::from(format!("/tmp/vcs-status-daemon-{user}")))
}

pub fn socket_path() -> Result<PathBuf> {
    Ok(runtime_dir()?.join("sock"))
}

pub fn pid_path() -> Result<PathBuf> {
    Ok(runtime_dir()?.join("pid"))
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

pub fn config_path() -> Option<PathBuf> {
    // Check VSD_CONFIG_FILE env var first
    if let Ok(path) = std::env::var("VSD_CONFIG_FILE")
        && !path.is_empty()
    {
        return Some(PathBuf::from(path));
    }

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

# How many ancestors of @ to search for bookmarks (jj only).
# bookmark_search_depth = 10

# Whether to include ANSI color codes in the output.
# Set to false if your shell prompt handles colors separately.
# color = true

# How long (ms) to wait for a fresh status before returning "not ready" or stale data.
# The daemon holds the client connection open until the scan completes or the timeout
# expires. Useful to avoid the initial "…" flash on first prompt.
# Set to 0 for immediate response without waiting.
# query_timeout_ms = 150

# --------------------------------------------------------------------------
# Templates
# --------------------------------------------------------------------------
# The status output is rendered using the Tera template engine (Jinja2-like).
#
# Built-in templates: "ascii" (default), "nerdfont", "unicode", "simple"
# Select one with template_name, or define your own below.

# Which template to use. Built-in options: "ascii", "nerdfont", "unicode", "simple"
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
#   commit_id_prefix               — unique-prefix portion of commit_id (for coloring)
#   commit_id_rest                 — remainder after the unique prefix
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
#   change_id_prefix               — unique-prefix portion of change_id (for coloring)
#   change_id_rest                 — remainder after the unique prefix
#   bookmarks                      — list of { name, distance, display }
#   divergent, hidden, immutable   — booleans
#
# git-only:
#   branch                         — current branch name
#   rebasing                       — true during a rebase
#
# Color filters (applied with | syntax, e.g. {{ branch | green }}):
#   red, green, yellow, blue, magenta, cyan, white
#   bright_red, bright_green, bright_yellow, bright_blue,
#   bright_magenta, bright_cyan, bright_white
#   bold, dim
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
    load_config_from(None)
}

pub fn load_config_from(config_file: Option<&Path>) -> Result<Config> {
    let Some(path) = config_file.map(|p| p.to_path_buf()).or_else(config_path) else {
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
format = "{{ change_id }}"
bookmark_search_depth = 5
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.idle_timeout_secs, 7200);
        assert_eq!(config.format, Some("{{ change_id }}".to_string()));
        assert_eq!(config.bookmark_search_depth, 5);
    }

    #[test]
    fn test_load_config_missing_file() {
        let config = load_config().unwrap();
        assert_eq!(config.idle_timeout_secs, 3600);
    }

    #[test]
    fn test_not_ready_format_default() {
        let config = Config::default();
        let fmt = config.resolved_not_ready_format();
        assert!(fmt.contains("…"), "default not-ready should contain …");
    }

    #[test]
    fn test_not_ready_format_nerdfont() {
        let toml_str = r#"template_name = "nerdfont""#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let fmt = config.resolved_not_ready_format();
        assert!(fmt.contains("…"), "nerdfont not-ready should contain …");
    }

    #[test]
    fn test_not_ready_format_custom() {
        let toml_str = r#"not_ready_format = "{{ \"wait\" | yellow }}""#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(
            config.resolved_not_ready_format(),
            "{{ \"wait\" | yellow }}"
        );
    }

    #[test]
    fn test_config_set_and_get() {
        let dir = tempfile::TempDir::with_prefix("vcs-cfg-set-").unwrap();
        let cf = dir.path().join("config.toml");
        std::fs::write(&cf, "template_name = \"ascii\"\n").unwrap();

        crate::run_config(
            crate::ConfigAction::Set {
                key: "template_name".into(),
                value: "nerdfont".into(),
            },
            Some(&cf),
        )
        .unwrap();

        // Verify via load_config_from
        let config = load_config_from(Some(&cf)).unwrap();
        assert_eq!(config.template_name, "nerdfont");

        // Verify the file is still valid TOML
        let contents = std::fs::read_to_string(&cf).unwrap();
        let _: Config = toml::from_str(&contents).unwrap();
    }

    #[test]
    fn test_config_set_integer() {
        let dir = tempfile::TempDir::with_prefix("vcs-cfg-int-").unwrap();
        let cf = dir.path().join("config.toml");
        std::fs::write(&cf, "").unwrap();

        crate::run_config(
            crate::ConfigAction::Set {
                key: "idle_timeout_secs".into(),
                value: "500".into(),
            },
            Some(&cf),
        )
        .unwrap();

        let config = load_config_from(Some(&cf)).unwrap();
        assert_eq!(config.idle_timeout_secs, 500);
    }

    #[test]
    fn test_config_set_bool() {
        let dir = tempfile::TempDir::with_prefix("vcs-cfg-bool-").unwrap();
        let cf = dir.path().join("config.toml");
        std::fs::write(&cf, "").unwrap();

        crate::run_config(
            crate::ConfigAction::Set {
                key: "color".into(),
                value: "false".into(),
            },
            Some(&cf),
        )
        .unwrap();

        let config = load_config_from(Some(&cf)).unwrap();
        assert!(!config.color);
    }

    #[test]
    fn test_config_path() {
        let dir = tempfile::TempDir::with_prefix("vcs-cfg-path-").unwrap();
        let cf = dir.path().join("config.toml");
        std::fs::write(&cf, "").unwrap();

        // Just verify it doesn't error
        let result = crate::run_config(crate::ConfigAction::Path, Some(&cf));
        assert!(result.is_ok());
    }

    #[test]
    fn test_config_init() {
        let dir = tempfile::TempDir::with_prefix("vcs-cfg-init-").unwrap();
        let cf = dir.path().join("subdir").join("config.toml");

        assert!(!cf.exists());

        crate::run_config(crate::ConfigAction::Init, Some(&cf)).unwrap();

        // File should now exist and be valid
        assert!(cf.exists());
        let contents = std::fs::read_to_string(&cf).unwrap();
        let _: Config = toml::from_str(&contents).unwrap();

        // Running init again should fail (file already exists)
        let result = crate::run_config(crate::ConfigAction::Init, Some(&cf));
        assert!(result.is_err(), "config init should fail when file exists");
    }

    #[test]
    fn test_config_set_preserves_comments() {
        let dir = tempfile::TempDir::with_prefix("vcs-cfg-comments-").unwrap();
        let cf = dir.path().join("config.toml");
        std::fs::write(&cf, DEFAULT_CONFIG_TOML).unwrap();

        crate::run_config(
            crate::ConfigAction::Set {
                key: "template_name".into(),
                value: "simple".into(),
            },
            Some(&cf),
        )
        .unwrap();

        let contents = std::fs::read_to_string(&cf).unwrap();
        assert!(
            contents.contains("# vcs-status-daemon configuration"),
            "comments should be preserved"
        );
        assert!(
            contents.contains("template_name = \"simple\""),
            "new value should be present"
        );
    }

    #[test]
    fn test_config_get_defaults_without_file() {
        let dir = tempfile::TempDir::with_prefix("vcs-cfg-nofile-").unwrap();
        let cf = dir.path().join("nonexistent.toml");

        // File doesn't exist — should return defaults
        let config = load_config_from(Some(&cf)).unwrap();
        assert_eq!(config.template_name, "ascii");
    }

    #[test]
    fn test_config_set_rejects_unknown_key() {
        let dir = tempfile::TempDir::with_prefix("vcs-cfg-badkey-").unwrap();
        let cf = dir.path().join("config.toml");
        std::fs::write(&cf, "").unwrap();

        let result = crate::run_config(
            crate::ConfigAction::Set {
                key: "bogus_key".into(),
                value: "hello".into(),
            },
            Some(&cf),
        );
        assert!(result.is_err(), "config set should reject unknown keys");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("bogus_key"),
            "error should mention the bad key: {err}"
        );
    }

    #[test]
    fn test_config_get_rejects_unknown_key() {
        let dir = tempfile::TempDir::with_prefix("vcs-cfg-badget-").unwrap();
        let cf = dir.path().join("config.toml");
        std::fs::write(&cf, "").unwrap();

        let result = crate::run_config(
            crate::ConfigAction::Get {
                key: "nonexistent".into(),
            },
            Some(&cf),
        );
        assert!(result.is_err(), "config get should reject unknown keys");
    }

    #[test]
    fn test_config_set_rejects_wrong_type() {
        let dir = tempfile::TempDir::with_prefix("vcs-cfg-badtype-").unwrap();
        let cf = dir.path().join("config.toml");
        std::fs::write(&cf, "").unwrap();

        let result = crate::run_config(
            crate::ConfigAction::Set {
                key: "idle_timeout_secs".into(),
                value: "notanumber".into(),
            },
            Some(&cf),
        );
        assert!(result.is_err(), "config set should reject wrong types");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("idle_timeout_secs"),
            "error should mention the key: {err}"
        );
    }

    #[test]
    fn test_config_set_rejects_invalid_template_name() {
        let dir = tempfile::TempDir::with_prefix("vcs-cfg-badtmpl-").unwrap();
        let cf = dir.path().join("config.toml");
        std::fs::write(&cf, "").unwrap();

        let result = crate::run_config(
            crate::ConfigAction::Set {
                key: "template_name".into(),
                value: "nonexistent".into(),
            },
            Some(&cf),
        );
        assert!(
            result.is_err(),
            "config set should reject unknown template names"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("nonexistent"),
            "error should mention the bad name: {err}"
        );
        assert!(
            err.contains("ascii"),
            "error should list valid names: {err}"
        );
    }

    #[test]
    fn test_config_set_accepts_user_defined_template_name() {
        let dir = tempfile::TempDir::with_prefix("vcs-cfg-usertmpl-").unwrap();
        let cf = dir.path().join("config.toml");
        std::fs::write(&cf, "[templates]\nmy_custom = \"{{ change_id }}\"\n").unwrap();

        crate::run_config(
            crate::ConfigAction::Set {
                key: "template_name".into(),
                value: "my_custom".into(),
            },
            Some(&cf),
        )
        .unwrap();
    }
}
