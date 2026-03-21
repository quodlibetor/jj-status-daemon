use serde::Serialize;
use tera::Tera;

/// Built-in "ascii" template — works in any terminal.
///
/// jj: `JJ xlvlt main [3 +10-5]`
/// git: `+- main abc1234 [3 +10-5]`
pub const ASCII_FORMAT: &str = include_str!("templates/ascii.tera");

/// Built-in "nerdfont" template — requires a Nerd Font.
///
/// jj: `󱗆 xlvlt  main [3 +10 -5]`
/// git: ` main abc1234 [3 +10 -5]`
pub const NERDFONT_FORMAT: &str = include_str!("templates/nerdfont.tera");

/// Built-in "unicode" template — uses Unicode symbols (no Nerd Fonts needed).
///
/// jj: `※ xlvlt ≡ main [3 +10-5]`
/// git: `± main abc1234 [3 +10-5]`
pub const UNICODE_FORMAT: &str = include_str!("templates/unicode.tera");

/// Built-in "simple" template — just branch/bookmark, color-coded by dirty state.
///
/// Clean: green branch name. Dirty: yellow branch name.
/// jj: `main` or `xlvlt`  git: `main`
pub const SIMPLE_FORMAT: &str = include_str!("templates/simple.tera");

/// Built-in "not ready" template for when the daemon hasn't cached status yet.
/// Only color variables are available — no repo status values.
pub const NOT_READY_ASCII: &str = include_str!("templates/not_ready.tera");
pub const NOT_READY_NERDFONT: &str = include_str!("templates/not_ready.tera");

#[derive(Debug, Clone, Default, Serialize)]
pub struct Bookmark {
    pub name: String,
    pub distance: u32,
    /// Pre-formatted display string: "main" or "main+2"
    pub display: String,
}

#[derive(Debug, Clone)]
pub struct RepoStatus {
    // VCS type flags
    pub is_jj: bool,
    pub is_git: bool,

    // Shared
    pub commit_id: String,
    pub description: String,
    pub empty: bool,
    pub conflict: bool,

    // Unstaged changes (working tree vs index for git, full @ diff for jj)
    pub files_changed: u32,
    pub lines_added: u32,
    pub lines_removed: u32,

    // Staged changes (index vs HEAD for git, always 0 for jj)
    pub staged_files_changed: u32,
    pub staged_lines_added: u32,
    pub staged_lines_removed: u32,

    // Total changes (working tree vs HEAD for git, same as unstaged for jj)
    pub total_files_changed: u32,
    pub total_lines_added: u32,
    pub total_lines_removed: u32,

    // jj-specific
    pub change_id: String,
    /// Number of reverse-hex chars that form the shortest unique prefix
    /// (for colorized display). Defaults to the full length of `change_id`.
    pub change_id_prefix_len: usize,
    /// Number of hex chars that form the shortest unique prefix for commit_id.
    pub commit_id_prefix_len: usize,
    pub bookmarks: Vec<Bookmark>,
    pub divergent: bool,
    pub hidden: bool,
    pub immutable: bool,

    // git-specific
    pub branch: String,
    pub rebasing: bool,

    // Workspace/worktree
    pub workspace_name: String,
    pub is_default_workspace: bool,

    // Staleness (set when refresh fails with cached data)
    pub is_stale: bool,
    pub refresh_error: String,
}

impl Default for RepoStatus {
    fn default() -> Self {
        Self {
            is_jj: false,
            is_git: false,
            commit_id: String::new(),
            description: String::new(),
            empty: false,
            conflict: false,
            files_changed: 0,
            lines_added: 0,
            lines_removed: 0,
            staged_files_changed: 0,
            staged_lines_added: 0,
            staged_lines_removed: 0,
            total_files_changed: 0,
            total_lines_added: 0,
            total_lines_removed: 0,
            change_id: String::new(),
            change_id_prefix_len: usize::MAX,
            commit_id_prefix_len: usize::MAX,
            bookmarks: Vec::new(),
            divergent: false,
            hidden: false,
            immutable: false,
            branch: String::new(),
            rebasing: false,
            workspace_name: String::new(),
            is_default_workspace: true,
            is_stale: false,
            refresh_error: String::new(),
        }
    }
}

/// Build a color filter function that wraps its input in ANSI codes.
/// When `color` is false, the filter is a no-op (returns input unchanged).
fn make_color_filter(code: &'static str, color: bool) -> impl tera::Filter + 'static {
    move |value: &tera::Value,
          _args: &std::collections::HashMap<String, tera::Value>|
          -> tera::Result<tera::Value> {
        let s = match value {
            tera::Value::String(s) => s.clone(),
            tera::Value::Number(n) => n.to_string(),
            tera::Value::Bool(b) => b.to_string(),
            other => other.to_string(),
        };
        if color {
            Ok(tera::Value::String(format!("{code}{s}\x1b[0m")))
        } else {
            Ok(tera::Value::String(s))
        }
    }
}

/// Create a Tera instance with color filters registered.
fn build_tera(template: &str, color: bool) -> Result<Tera, tera::Error> {
    let mut tera = Tera::default();
    tera.add_raw_template("tpl", template)?;

    // Register a filter for each color name (lowercase).
    // Usage: {{ branch | green }}, {{ "CONFLICT" | bright_red }}
    let colors: &[(&str, &str)] = &[
        ("bold", "\x1b[1m"),
        ("dim", "\x1b[2m"),
        ("black", "\x1b[30m"),
        ("red", "\x1b[31m"),
        ("green", "\x1b[32m"),
        ("yellow", "\x1b[33m"),
        ("blue", "\x1b[34m"),
        ("magenta", "\x1b[35m"),
        ("cyan", "\x1b[36m"),
        ("white", "\x1b[37m"),
        ("bright_black", "\x1b[90m"),
        ("bright_red", "\x1b[91m"),
        ("bright_green", "\x1b[92m"),
        ("bright_yellow", "\x1b[93m"),
        ("bright_blue", "\x1b[94m"),
        ("bright_magenta", "\x1b[95m"),
        ("bright_cyan", "\x1b[96m"),
        ("bright_white", "\x1b[97m"),
    ];
    for &(name, code) in colors {
        tera.register_filter(name, make_color_filter(code, color));
    }

    Ok(tera)
}

pub fn format_status(status: &RepoStatus, template: &str, color: bool) -> String {
    let mut ctx = tera::Context::new();

    // VCS type
    ctx.insert("is_jj", &status.is_jj);
    ctx.insert("is_git", &status.is_git);

    // Shared
    ctx.insert("commit_id", &status.commit_id);
    ctx.insert("description", &status.description);
    ctx.insert("empty", &status.empty);
    ctx.insert("conflict", &status.conflict);

    // Unstaged changes (working tree vs index for git, full @ diff for jj)
    ctx.insert("files_changed", &status.files_changed);
    ctx.insert("lines_added", &status.lines_added);
    ctx.insert("lines_removed", &status.lines_removed);

    // Staged changes (index vs HEAD for git, always 0 for jj)
    ctx.insert("staged_files_changed", &status.staged_files_changed);
    ctx.insert("staged_lines_added", &status.staged_lines_added);
    ctx.insert("staged_lines_removed", &status.staged_lines_removed);

    // Total changes (working tree vs HEAD for git, same as unstaged for jj)
    ctx.insert("total_files_changed", &status.total_files_changed);
    ctx.insert("total_lines_added", &status.total_lines_added);
    ctx.insert("total_lines_removed", &status.total_lines_removed);

    // jj-specific
    ctx.insert("change_id", &status.change_id);
    let change_pfx = status.change_id_prefix_len.min(status.change_id.len());
    ctx.insert("change_id_prefix", &status.change_id[..change_pfx]);
    ctx.insert("change_id_rest", &status.change_id[change_pfx..]);
    let commit_pfx = status.commit_id_prefix_len.min(status.commit_id.len());
    ctx.insert("commit_id_prefix", &status.commit_id[..commit_pfx]);
    ctx.insert("commit_id_rest", &status.commit_id[commit_pfx..]);
    ctx.insert("bookmarks", &status.bookmarks);
    ctx.insert("has_bookmarks", &!status.bookmarks.is_empty());
    ctx.insert("divergent", &status.divergent);
    ctx.insert("hidden", &status.hidden);
    ctx.insert("immutable", &status.immutable);

    // git-specific
    ctx.insert("branch", &status.branch);
    ctx.insert("has_branch", &!status.branch.is_empty());
    ctx.insert("rebasing", &status.rebasing);

    // Workspace/worktree
    ctx.insert("workspace_name", &status.workspace_name);
    ctx.insert("is_default_workspace", &status.is_default_workspace);

    // Staleness
    ctx.insert("is_stale", &status.is_stale);
    ctx.insert("refresh_error", &status.refresh_error);

    match build_tera(template, color) {
        Ok(tera) => match tera.render("tpl", &ctx) {
            Ok(rendered) => rendered.trim().to_string(),
            Err(e) => format!("template error: {e}"),
        },
        Err(e) => format!("template error: {e}"),
    }
}

/// Validate a Tera template by rendering it with dummy data.
/// Returns `Ok(())` if the template is valid, or `Err(message)` if not.
pub fn validate_template(template: &str) -> Result<(), String> {
    let status = RepoStatus::default();
    let rendered = format_status(&status, template, false);
    if rendered.starts_with("template error:") {
        Err(rendered)
    } else {
        Ok(())
    }
}

/// All built-in template names, in display order.
pub const BUILTIN_NAMES: &[&str] = &["ascii", "nerdfont", "unicode", "simple"];

/// Representative sample statuses for template previews.
pub fn sample_statuses() -> Vec<(&'static str, RepoStatus)> {
    vec![
        (
            "jj: clean, on bookmark",
            RepoStatus {
                is_jj: true,
                change_id: "xlvltmpk".into(),
                change_id_prefix_len: 2,
                commit_id: "abc12345".into(),
                description: "refactor auth".into(),
                empty: true,
                bookmarks: vec![Bookmark {
                    name: "main".into(),
                    distance: 0,
                    display: "main".into(),
                }],
                ..Default::default()
            },
        ),
        (
            "jj: dirty, bookmark ahead",
            RepoStatus {
                is_jj: true,
                change_id: "mrtunzqw".into(),
                change_id_prefix_len: 2,
                commit_id: "def23456".into(),
                description: "add tests".into(),
                bookmarks: vec![Bookmark {
                    name: "main".into(),
                    distance: 2,
                    display: "main+2".into(),
                }],
                files_changed: 3,
                lines_added: 10,
                lines_removed: 5,
                total_files_changed: 3,
                total_lines_added: 10,
                total_lines_removed: 5,
                ..Default::default()
            },
        ),
        (
            "jj: new, working",
            RepoStatus {
                is_jj: true,
                change_id: "qstvwxyz".into(),
                change_id_prefix_len: 2,
                commit_id: "mno45678".into(),
                bookmarks: vec![Bookmark {
                    name: "main".into(),
                    distance: 1,
                    display: "main+1".into(),
                }],
                files_changed: 2,
                lines_added: 8,
                lines_removed: 1,
                total_files_changed: 2,
                total_lines_added: 8,
                total_lines_removed: 1,
                ..Default::default()
            },
        ),
        (
            "jj: conflict",
            RepoStatus {
                is_jj: true,
                change_id: "npqrsvyx".into(),
                change_id_prefix_len: 2,
                commit_id: "ghi34567".into(),
                conflict: true,
                empty: true,
                bookmarks: vec![Bookmark {
                    name: "feat".into(),
                    distance: 0,
                    display: "feat".into(),
                }],
                ..Default::default()
            },
        ),
        (
            "jj: divergent",
            RepoStatus {
                is_jj: true,
                change_id: "wkqolyzp".into(),
                change_id_prefix_len: 2,
                commit_id: "jkl45678".into(),
                divergent: true,
                empty: true,
                bookmarks: vec![Bookmark {
                    name: "main".into(),
                    distance: 1,
                    display: "main+1".into(),
                }],
                ..Default::default()
            },
        ),
        (
            "jj: named workspace",
            RepoStatus {
                is_jj: true,
                change_id: "bfglmprs".into(),
                change_id_prefix_len: 2,
                commit_id: "pqr56789".into(),
                empty: true,
                bookmarks: vec![Bookmark {
                    name: "main".into(),
                    distance: 0,
                    display: "main".into(),
                }],
                workspace_name: "secondary".into(),
                is_default_workspace: false,
                ..Default::default()
            },
        ),
        (
            "git: clean",
            RepoStatus {
                is_git: true,
                branch: "main".into(),
                commit_id: "abc1234".into(),
                description: "initial commit".into(),
                ..Default::default()
            },
        ),
        (
            "git: staged only",
            RepoStatus {
                is_git: true,
                branch: "feature".into(),
                commit_id: "def5678".into(),
                description: "wip".into(),
                staged_files_changed: 2,
                staged_lines_added: 15,
                staged_lines_removed: 3,
                total_files_changed: 2,
                total_lines_added: 15,
                total_lines_removed: 3,
                ..Default::default()
            },
        ),
        (
            "git: unstaged changes",
            RepoStatus {
                is_git: true,
                branch: "develop".into(),
                commit_id: "789abcd".into(),
                description: "fix bug".into(),
                files_changed: 1,
                lines_added: 4,
                lines_removed: 2,
                total_files_changed: 1,
                total_lines_added: 4,
                total_lines_removed: 2,
                ..Default::default()
            },
        ),
        (
            "git: rebasing",
            RepoStatus {
                is_git: true,
                branch: "feature".into(),
                commit_id: "uvw8901".into(),
                description: "wip".into(),
                rebasing: true,
                conflict: true,
                ..Default::default()
            },
        ),
        (
            "git: linked worktree",
            RepoStatus {
                is_git: true,
                branch: "hotfix".into(),
                commit_id: "stu7890".into(),
                description: "urgent fix".into(),
                workspace_name: "hotfix-wt".into(),
                is_default_workspace: false,
                ..Default::default()
            },
        ),
        (
            "jj: stale (refresh error)",
            RepoStatus {
                is_jj: true,
                change_id: "xlvltmpk".into(),
                change_id_prefix_len: 2,
                commit_id: "abc12345".into(),
                bookmarks: vec![Bookmark {
                    name: "main".into(),
                    distance: 0,
                    display: "main".into(),
                }],
                is_stale: true,
                refresh_error: "jj exited with status 1".into(),
                ..Default::default()
            },
        ),
    ]
}

/// Look up a built-in template by name.
pub fn builtin_template(name: &str) -> Option<&'static str> {
    match name {
        "ascii" => Some(ASCII_FORMAT),
        "nerdfont" => Some(NERDFONT_FORMAT),
        "unicode" => Some(UNICODE_FORMAT),
        "simple" => Some(SIMPLE_FORMAT),
        _ => None,
    }
}

/// Look up a built-in not-ready template by name.
pub fn builtin_not_ready_template(name: &str) -> &'static str {
    match name {
        "nerdfont" => NOT_READY_NERDFONT,
        _ => NOT_READY_ASCII,
    }
}

/// Render a "not ready" template with only color variables available.
pub fn format_not_ready(template: &str, color: bool) -> String {
    let ctx = tera::Context::new();
    match build_tera(template, color) {
        Ok(tera) => match tera.render("tpl", &ctx) {
            Ok(rendered) => rendered.trim().to_string(),
            Err(e) => format!("template error: {e}"),
        },
        Err(e) => format!("template error: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[test]
    fn test_format_jj_with_metrics() {
        let status = RepoStatus {
            is_jj: true,
            change_id: "mrtu".to_string(),
            commit_id: "abc1".to_string(),
            description: "test".to_string(),
            empty: false,
            bookmarks: vec![Bookmark {
                name: "main".into(),
                distance: 0,
                display: "main".into(),
            }],
            files_changed: 3,
            lines_added: 10,
            lines_removed: 5,
            total_files_changed: 3,
            total_lines_added: 10,
            total_lines_removed: 5,
            ..Default::default()
        };
        let formatted = format_status(&status, ASCII_FORMAT, false);
        assert_eq!(formatted, "JJ mrtu main [3 +10-5]");
    }

    #[test]
    fn test_format_jj_empty() {
        let status = RepoStatus {
            is_jj: true,
            change_id: "mrtu".to_string(),
            commit_id: "abc1".to_string(),
            empty: true,
            ..Default::default()
        };
        let formatted = format_status(&status, ASCII_FORMAT, false);
        assert_eq!(formatted, "JJ mrtu (EMPTY)");
    }

    #[test]
    fn test_format_git_with_metrics() {
        let status = RepoStatus {
            is_git: true,
            branch: "main".to_string(),
            commit_id: "abc1234".to_string(),
            total_files_changed: 3,
            total_lines_added: 10,
            total_lines_removed: 5,
            ..Default::default()
        };
        let formatted = format_status(&status, ASCII_FORMAT, false);
        assert_eq!(formatted, "+- main abc1234 [3 +10-5]");
    }

    #[test]
    fn test_format_git_empty() {
        let status = RepoStatus {
            is_git: true,
            branch: "main".to_string(),
            commit_id: "abc1234".to_string(),
            empty: true,
            ..Default::default()
        };
        let formatted = format_status(&status, ASCII_FORMAT, false);
        assert_eq!(formatted, "+- main abc1234 (EMPTY)");
    }

    #[test]
    fn test_format_custom_template() {
        let status = RepoStatus {
            change_id: "mrtu".to_string(),
            commit_id: "abc1".to_string(),
            description: "my change".to_string(),
            bookmarks: vec![Bookmark {
                name: "main".into(),
                distance: 0,
                display: "main".into(),
            }],
            ..Default::default()
        };
        let tmpl = "{{ commit_id }}:{{ change_id }} {{ description }}";
        let formatted = format_status(&status, tmpl, false);
        assert_eq!(formatted, "abc1:mrtu my change");
    }

    #[test]
    fn test_format_toml_multiline_matches_default() {
        let toml_str = r#"
format = '''
{% if is_jj %}{{ "JJ" | magenta }} {{ change_id_prefix | magenta | bold }}{{ change_id_rest | bright_black }}
{%- for b in bookmarks %} {{ b.display | blue }}{% endfor %}
{%- elif is_git %}{{ "+" | green }}{{ "-" | red }} {{ branch | blue }} {{ commit_id_prefix | blue | bold }}{{ commit_id_rest | bright_black }}
{%- endif %}
{%- if total_files_changed > 0 %} {{ "[" | blue }}{{ total_files_changed | bright_blue }} {{ "+" | bright_green }}{{ total_lines_added | bright_green }}{{ "-" | bright_red }}{{ total_lines_removed | bright_red }}{{ "]" | blue }}{% endif %}
{%- if conflict %} {{ "CONFLICT" | bright_red }}{% endif %}
{%- if divergent %} {{ "DIVERGENT" | bright_red }}{% endif %}
{%- if hidden %} {{ "HIDDEN" | bright_yellow }}{% endif %}
{%- if immutable %} {{ "IMMUTABLE" | yellow }}{% endif %}
{%- if empty %} {{ "(" | blue }}EMPTY{{ ")" | blue }}{% endif %}
{%- if not is_default_workspace %} {{ "/" | bright_green }}{{ workspace_name }}{{ "\\" | bright_green }}{% endif %}'''
"#;
        let config: Config = toml::from_str(toml_str).unwrap();

        let cases = [
            RepoStatus {
                is_jj: true,
                change_id: "mrtu".into(),
                empty: true,
                ..Default::default()
            },
            RepoStatus {
                is_jj: true,
                change_id: "mrtu".into(),
                bookmarks: vec![Bookmark {
                    name: "main".into(),
                    distance: 0,
                    display: "main".into(),
                }],
                total_files_changed: 3,
                total_lines_added: 10,
                total_lines_removed: 5,
                ..Default::default()
            },
            RepoStatus {
                is_jj: true,
                change_id: "abcd".into(),
                bookmarks: vec![
                    Bookmark {
                        name: "feat".into(),
                        distance: 0,
                        display: "feat".into(),
                    },
                    Bookmark {
                        name: "main".into(),
                        distance: 2,
                        display: "main+2".into(),
                    },
                ],
                total_files_changed: 1,
                total_lines_added: 7,
                total_lines_removed: 0,
                ..Default::default()
            },
            RepoStatus {
                is_jj: true,
                change_id: "zzzz".into(),
                conflict: true,
                empty: true,
                ..Default::default()
            },
            RepoStatus {
                is_git: true,
                branch: "main".into(),
                commit_id: "abc1234".into(),
                total_files_changed: 3,
                total_lines_added: 10,
                total_lines_removed: 5,
                ..Default::default()
            },
            RepoStatus {
                is_git: true,
                branch: "develop".into(),
                commit_id: "def5678".into(),
                empty: true,
                ..Default::default()
            },
        ];

        for (i, status) in cases.iter().enumerate() {
            let from_default = format_status(status, ASCII_FORMAT, false);
            let from_toml = format_status(status, &config.resolved_format(), false);
            assert_eq!(
                from_default, from_toml,
                "case {i}: ASCII_FORMAT and TOML multi-line produced different output\n  default: {from_default:?}\n  toml:    {from_toml:?}"
            );
        }
    }

    #[test]
    fn test_format_conflict_state() {
        let status = RepoStatus {
            is_jj: true,
            change_id: "mrtu".to_string(),
            conflict: true,
            ..Default::default()
        };
        let formatted = format_status(&status, ASCII_FORMAT, false);
        assert!(formatted.contains("CONFLICT"));
    }

    #[test]
    fn test_nerdfont_jj() {
        let status = RepoStatus {
            is_jj: true,
            change_id: "mrtu".to_string(),
            bookmarks: vec![Bookmark {
                name: "main".into(),
                distance: 0,
                display: "main".into(),
            }],
            total_files_changed: 3,
            total_lines_added: 10,
            total_lines_removed: 5,
            ..Default::default()
        };
        let formatted = format_status(&status, NERDFONT_FORMAT, false);
        assert!(formatted.contains("󱗆"), "expected jj icon: {formatted:?}");
        assert!(
            formatted.contains("mrtu"),
            "expected change_id: {formatted:?}"
        );
        assert!(
            formatted.contains(" main"),
            "expected bookmark icon: {formatted:?}"
        );
        assert!(
            formatted.contains("+10"),
            "expected additions: {formatted:?}"
        );
    }

    #[test]
    fn test_nerdfont_git() {
        let status = RepoStatus {
            is_git: true,
            branch: "main".to_string(),
            commit_id: "abc1234".to_string(),
            total_files_changed: 2,
            total_lines_added: 7,
            total_lines_removed: 3,
            ..Default::default()
        };
        let formatted = format_status(&status, NERDFONT_FORMAT, false);
        assert!(formatted.contains(""), "expected git icon: {formatted:?}");
        assert!(formatted.contains("main"), "expected branch: {formatted:?}");
        assert!(
            formatted.contains("abc1234"),
            "expected commit_id: {formatted:?}",
        );
    }

    #[test]
    fn test_nerdfont_empty() {
        let status = RepoStatus {
            is_jj: true,
            change_id: "mrtu".to_string(),
            empty: true,
            ..Default::default()
        };
        let formatted = format_status(&status, NERDFONT_FORMAT, false);
        assert!(
            formatted.contains("∅"),
            "expected empty symbol: {formatted:?}"
        );
        assert!(
            !formatted.contains("EMPTY"),
            "nerdfont should use ∅ not EMPTY: {formatted:?}"
        );
    }

    #[test]
    fn test_unicode_jj() {
        let status = RepoStatus {
            is_jj: true,
            change_id: "mrtu".to_string(),
            bookmarks: vec![Bookmark {
                name: "main".into(),
                distance: 0,
                display: "main".into(),
            }],
            total_files_changed: 3,
            total_lines_added: 10,
            total_lines_removed: 5,
            ..Default::default()
        };
        let formatted = format_status(&status, UNICODE_FORMAT, false);
        assert!(
            formatted.contains("\u{22C8}"),
            "expected bowtie: {formatted:?}"
        );
        assert!(
            formatted.contains("mrtu"),
            "expected change_id: {formatted:?}"
        );
        assert!(
            formatted.contains("\u{2261}"),
            "expected equiv sign for bookmark: {formatted:?}"
        );
        assert!(
            formatted.contains("main"),
            "expected bookmark: {formatted:?}"
        );
        assert!(
            formatted.contains("["),
            "expected left bracket: {formatted:?}"
        );
    }

    #[test]
    fn test_unicode_git() {
        let status = RepoStatus {
            is_git: true,
            branch: "main".to_string(),
            commit_id: "abc1234".to_string(),
            ..Default::default()
        };
        let formatted = format_status(&status, UNICODE_FORMAT, false);
        assert!(
            formatted.contains("\u{00B1}"),
            "expected plus-minus sign: {formatted:?}"
        );
        assert!(formatted.contains("main"), "expected branch: {formatted:?}");
    }

    #[test]
    fn test_unicode_workspace() {
        let status = RepoStatus {
            is_jj: true,
            change_id: "mrtu".to_string(),
            workspace_name: "secondary".to_string(),
            is_default_workspace: false,
            ..Default::default()
        };
        let formatted = format_status(&status, UNICODE_FORMAT, false);
        assert!(
            formatted.contains("\u{6728}"),
            "expected kanji tree: {formatted:?}"
        );
        assert!(
            formatted.contains("secondary"),
            "expected workspace name: {formatted:?}"
        );
    }

    #[test]
    fn test_simple_jj_on_bookmark() {
        // Case 1: on a bookmark → always green, show bookmark name
        let status = RepoStatus {
            is_jj: true,
            change_id: "mrtu".to_string(),
            files_changed: 3, // dirty doesn't matter — still green
            bookmarks: vec![Bookmark {
                name: "main".into(),
                distance: 0,
                display: "main".into(),
            }],
            ..Default::default()
        };
        let formatted = format_status(&status, SIMPLE_FORMAT, true);
        assert!(
            formatted.contains("main"),
            "expected bookmark: {formatted:?}"
        );
        assert!(
            formatted.contains("\x1b[32m"),
            "expected green ANSI: {formatted:?}"
        );
    }

    #[test]
    fn test_simple_jj_described() {
        // Case 1: has description, no bookmark → always green, show description
        let status = RepoStatus {
            is_jj: true,
            change_id: "mrtu".to_string(),
            description: "fix auth".to_string(),
            files_changed: 2,
            ..Default::default()
        };
        let formatted = format_status(&status, SIMPLE_FORMAT, true);
        assert!(
            formatted.contains("fix auth"),
            "expected description: {formatted:?}"
        );
        assert!(
            formatted.contains("\x1b[32m"),
            "expected green ANSI: {formatted:?}"
        );
    }

    #[test]
    fn test_simple_jj_one_ahead_empty() {
        // Case 2: undescribed, 1 ahead of bookmark, empty → green, show bookmark
        let status = RepoStatus {
            is_jj: true,
            change_id: "mrtu".to_string(),
            empty: true,
            bookmarks: vec![Bookmark {
                name: "main".into(),
                distance: 1,
                display: "main+1".into(),
            }],
            ..Default::default()
        };
        let formatted = format_status(&status, SIMPLE_FORMAT, true);
        assert!(
            formatted.contains("main"),
            "expected bookmark: {formatted:?}"
        );
        assert!(
            formatted.contains("\x1b[32m"),
            "expected green ANSI: {formatted:?}"
        );
    }

    #[test]
    fn test_simple_jj_one_ahead_dirty() {
        // Case 2: undescribed, 1 ahead of bookmark, has changes → yellow, show bookmark
        let status = RepoStatus {
            is_jj: true,
            change_id: "mrtu".to_string(),
            empty: false,
            files_changed: 2,
            bookmarks: vec![Bookmark {
                name: "main".into(),
                distance: 1,
                display: "main+1".into(),
            }],
            ..Default::default()
        };
        let formatted = format_status(&status, SIMPLE_FORMAT, true);
        assert!(
            formatted.contains("main"),
            "expected bookmark: {formatted:?}"
        );
        assert!(
            formatted.contains("\x1b[33m"),
            "expected yellow ANSI: {formatted:?}"
        );
    }

    #[test]
    fn test_simple_jj_no_nearby_bookmark_empty() {
        // Case 3: no bookmark or description, empty → green, show change_id
        let status = RepoStatus {
            is_jj: true,
            change_id: "mrtu".to_string(),
            empty: true,
            ..Default::default()
        };
        let formatted = format_status(&status, SIMPLE_FORMAT, true);
        assert!(
            formatted.contains("mrtu"),
            "expected change_id: {formatted:?}"
        );
        assert!(
            formatted.contains("\x1b[32m"),
            "expected green ANSI: {formatted:?}"
        );
    }

    #[test]
    fn test_simple_jj_no_nearby_bookmark_dirty() {
        // Case 3: no bookmark or description, has changes → yellow, show change_id
        let status = RepoStatus {
            is_jj: true,
            change_id: "mrtu".to_string(),
            empty: false,
            files_changed: 1,
            ..Default::default()
        };
        let formatted = format_status(&status, SIMPLE_FORMAT, true);
        assert!(
            formatted.contains("mrtu"),
            "expected change_id: {formatted:?}"
        );
        assert!(
            formatted.contains("\x1b[33m"),
            "expected yellow ANSI: {formatted:?}"
        );
    }

    #[test]
    fn test_simple_jj_far_bookmark_falls_through() {
        // Bookmark at distance 3, undescribed → case 3 (show change_id)
        let status = RepoStatus {
            is_jj: true,
            change_id: "mrtu".to_string(),
            empty: false,
            files_changed: 1,
            bookmarks: vec![Bookmark {
                name: "main".into(),
                distance: 3,
                display: "main+3".into(),
            }],
            ..Default::default()
        };
        let formatted = format_status(&status, SIMPLE_FORMAT, false);
        assert_eq!(formatted, "mrtu");
    }

    #[test]
    fn test_simple_git_clean() {
        let status = RepoStatus {
            is_git: true,
            branch: "main".to_string(),
            ..Default::default()
        };
        let formatted = format_status(&status, SIMPLE_FORMAT, false);
        assert_eq!(formatted, "main");
    }

    #[test]
    fn test_simple_git_unstaged() {
        let status = RepoStatus {
            is_git: true,
            branch: "develop".to_string(),
            files_changed: 2,
            total_files_changed: 2,
            ..Default::default()
        };
        let formatted = format_status(&status, SIMPLE_FORMAT, true);
        assert!(
            formatted.contains("develop"),
            "expected branch: {formatted:?}"
        );
        assert!(
            formatted.contains("\x1b[31m"),
            "expected red ANSI for unstaged: {formatted:?}"
        );
    }

    #[test]
    fn test_simple_git_staged_only() {
        let status = RepoStatus {
            is_git: true,
            branch: "feature".to_string(),
            staged_files_changed: 1,
            staged_lines_added: 3,
            total_files_changed: 1,
            total_lines_added: 3,
            ..Default::default()
        };
        let formatted = format_status(&status, SIMPLE_FORMAT, true);
        assert!(
            formatted.contains("feature"),
            "expected branch: {formatted:?}"
        );
        assert!(
            formatted.contains("\x1b[33m"),
            "expected yellow ANSI for staged: {formatted:?}"
        );
    }

    #[test]
    fn test_format_stale_ascii() {
        let status = RepoStatus {
            is_jj: true,
            change_id: "mrtu".to_string(),
            bookmarks: vec![Bookmark {
                name: "main".into(),
                distance: 0,
                display: "main".into(),
            }],
            is_stale: true,
            refresh_error: "jj-lib error".to_string(),
            ..Default::default()
        };
        let formatted = format_status(&status, ASCII_FORMAT, false);
        assert!(
            formatted.contains("STALE"),
            "expected STALE indicator: {formatted:?}"
        );
        // Original status data should still be present
        assert!(
            formatted.contains("mrtu"),
            "expected change_id preserved: {formatted:?}"
        );
    }

    #[test]
    fn test_format_stale_nerdfont() {
        let status = RepoStatus {
            is_jj: true,
            change_id: "mrtu".to_string(),
            is_stale: true,
            refresh_error: "error".to_string(),
            ..Default::default()
        };
        let formatted = format_status(&status, NERDFONT_FORMAT, false);
        assert!(
            formatted.contains("󰇘"),
            "expected nerdfont stale icon: {formatted:?}"
        );
    }

    #[test]
    fn test_format_stale_unicode() {
        let status = RepoStatus {
            is_git: true,
            branch: "main".to_string(),
            commit_id: "abc1234".to_string(),
            is_stale: true,
            refresh_error: "git2 error".to_string(),
            ..Default::default()
        };
        let formatted = format_status(&status, UNICODE_FORMAT, false);
        assert!(
            formatted.contains("⟳"),
            "expected unicode stale icon: {formatted:?}"
        );
        assert!(
            formatted.contains("main"),
            "expected branch preserved: {formatted:?}"
        );
    }

    #[test]
    fn test_format_not_stale_no_indicator() {
        let status = RepoStatus {
            is_jj: true,
            change_id: "mrtu".to_string(),
            is_stale: false,
            ..Default::default()
        };
        let formatted = format_status(&status, ASCII_FORMAT, false);
        assert!(
            !formatted.contains("STALE"),
            "non-stale status should not show STALE: {formatted:?}"
        );
    }

    #[test]
    fn test_format_stale_custom_template_with_error() {
        let status = RepoStatus {
            is_jj: true,
            change_id: "mrtu".to_string(),
            is_stale: true,
            refresh_error: "workspace load failed".to_string(),
            ..Default::default()
        };
        let tmpl = "{{ change_id }}{% if is_stale %} STALE({{ refresh_error }}){% endif %}";
        let formatted = format_status(&status, tmpl, false);
        assert_eq!(formatted, "mrtu STALE(workspace load failed)");
    }

    #[test]
    fn test_format_not_ready_no_color() {
        let formatted = format_not_ready(NOT_READY_ASCII, false);
        assert_eq!(formatted, "…");
    }

    #[test]
    fn test_format_not_ready_with_color() {
        let formatted = format_not_ready(NOT_READY_ASCII, true);
        assert!(formatted.contains("…"), "expected …: {formatted:?}");
        assert!(
            formatted.contains("\x1b["),
            "expected ANSI codes: {formatted:?}"
        );
    }

    #[test]
    fn test_format_not_ready_custom_template() {
        let tmpl = "{{ \"loading\" | yellow }}";
        let formatted = format_not_ready(tmpl, false);
        assert_eq!(formatted, "loading");

        let formatted = format_not_ready(tmpl, true);
        assert!(
            formatted.contains("\x1b[33m"),
            "expected yellow: {formatted:?}"
        );
        assert!(
            formatted.contains("loading"),
            "expected text: {formatted:?}"
        );
    }

    #[test]
    fn test_color_filters() {
        let status = RepoStatus {
            is_jj: true,
            change_id: "mrtu".to_string(),
            bookmarks: vec![Bookmark {
                name: "main".into(),
                distance: 0,
                display: "main".into(),
            }],
            conflict: true,
            ..Default::default()
        };
        let tmpl = r#"{{ change_id | blue }} {{ bookmarks[0].name | green }} {{ "CONFLICT" | bright_red }}"#;

        // With color off: filters are no-ops
        let formatted = format_status(&status, tmpl, false);
        assert_eq!(formatted, "mrtu main CONFLICT");

        // With color on: filters wrap in ANSI
        let formatted = format_status(&status, tmpl, true);
        assert!(
            formatted.contains("\x1b[34mmrtu\x1b[0m"),
            "expected blue change_id: {formatted:?}"
        );
        assert!(
            formatted.contains("\x1b[32mmain\x1b[0m"),
            "expected green bookmark: {formatted:?}"
        );
        assert!(
            formatted.contains("\x1b[91mCONFLICT\x1b[0m"),
            "expected bright_red CONFLICT: {formatted:?}"
        );
    }
}
