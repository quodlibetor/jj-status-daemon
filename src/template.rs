use serde::Serialize;
use tera::Tera;

/// Built-in "ascii" template — works in any terminal.
///
/// jj: `xlvlt main [3 +10-5]`
/// git: `main abc1234 [3 +10-5]`
pub const ASCII_FORMAT: &str = "\
{% if is_jj %}{{ MAGENTA }}JJ{{ RST }} {{ change_id }}\
{% for b in bookmarks %} {{ BLUE }}{{ b.display }}{{ RST }}{% endfor %}\
{% elif is_git %}{{ GREEN }}+{{ RED }}-{{ RST }} {{ BLUE }}{{ branch }}{{ RST }} {{ commit_id }}\
{% endif %}\
{% if total_files_changed > 0 %} {{ BLUE }}[{{ RST }}\
{{ BRIGHT_BLUE }}{{ total_files_changed }}{{ RST }} \
{{ BRIGHT_GREEN }}+{{ total_lines_added }}{{ RST }}\
{{ BRIGHT_RED }}-{{ total_lines_removed }}{{ RST }}\
{{ BLUE }}]{{ RST }}{% endif %}\
{% if conflict %} {{ BRIGHT_RED }}CONFLICT{{ RST }}{% endif %}\
{% if divergent %} {{ BRIGHT_RED }}DIVERGENT{{ RST }}{% endif %}\
{% if hidden %} {{ BRIGHT_YELLOW }}HIDDEN{{ RST }}{% endif %}\
{% if immutable %} {{ YELLOW }}IMMUTABLE{{ RST }}{% endif %}\
{% if empty %} {{ BLUE }}({{ RST }}EMPTY{{ BLUE }}){{ RST }}{% endif %}\
{% if not is_default_workspace %} {{ BRIGHT_GREEN }}/{{ RST }}{{ workspace_name }}{{ BRIGHT_GREEN }}\\{{ RST }}{% endif %}";

/// Built-in "nerdfont" template — requires a Nerd Font.
///
/// jj: `󱗆 xlvlt  main [3 +10 -5]`
/// git: ` main abc1234 [3 +10 -5]`
pub const NERDFONT_FORMAT: &str = "\
{% if is_jj %}{{ MAGENTA }}󱗆 {{ RST }}{{ change_id }}\
{% for b in bookmarks %} {{ BLUE }}{{ b.display }}{{ RST }}{% endfor %}\
{% elif is_git %}{{ BLUE }}\u{f02a2} {{ branch }}{{ RST }} {{ commit_id }}\
{% endif %}\
{% if total_files_changed > 0 %} {{ BLUE }}[{{ RST }}\
{{ BRIGHT_BLUE }}{{ total_files_changed }}{{ RST }} \
{{ BRIGHT_GREEN }}+{{ total_lines_added }}{{ RST }} \
{{ BRIGHT_RED }}-{{ total_lines_removed }}{{ RST }}\
{{ BLUE }}]{{ RST }}{% endif %}\
{% if conflict %} {{ BRIGHT_RED }}\u{f46e}{{ RST }}{% endif %}\
{% if divergent %} {{ BRIGHT_RED }}\u{f00fb}{{ RST }}{% endif %}\
{% if hidden %} {{ BRIGHT_YELLOW }}󰘌{{ RST }}{% endif %}\
{% if immutable %} {{ YELLOW }}{{ RST }}{% endif %}\
{% if empty %} {{ DIM }}∅{{ RST }}{% endif %}\
{% if not is_default_workspace %} {{ BRIGHT_GREEN }}\u{F0405} ({{ RST }}{{ workspace_name }}{{ BRIGHT_GREEN }}){{ RST }}{% endif %}";

/// Built-in "unicode" template — uses Unicode symbols (no Nerd Fonts needed).
///
/// jj: `\u{203B} xlvlt \u{2261} main \u{300C}3 +10\u{2212}5\u{300D}`
/// git: `\u{00B1} main abc1234 \u{300C}3 +10\u{2212}5\u{300D}`
pub const UNICODE_FORMAT: &str = "\
{% if is_jj %}{{ MAGENTA }}\u{203B}{{ RST }} {{ change_id }}\
{% for b in bookmarks %} {{ BLUE }}\u{2261} {{ b.display }}{{ RST }}{% endfor %}\
{% elif is_git %}{{ BLUE }}\u{00B1}{{ RST }} {{ BLUE }}{{ branch }}{{ RST }} {{ commit_id }}\
{% endif %}\
{% if total_files_changed > 0 %} {{ BLUE }}[{{ RST }}\
{{- BRIGHT_BLUE }}{{- total_files_changed }}{{ RST }} \
{{ BRIGHT_GREEN }}+{{ total_lines_added }}{{ RST }}\
{{ BRIGHT_RED }}-{{ total_lines_removed }}{{ RST }}\
{{ BLUE }}]{{ RST }}{% endif %}\
{% if conflict %} {{ BRIGHT_RED }}\u{2717}{{ RST }}{% endif %}\
{% if divergent %} {{ BRIGHT_RED }}\u{2ADD}{{ RST }}{% endif %}\
{% if hidden %} {{ BRIGHT_YELLOW }}\u{25CC}{{ RST }}{% endif %}\
{% if immutable %} {{ YELLOW }}\u{2205}{{ RST }}{% endif %}\
{% if empty %} {{ DIM }}\u{2205}{{ RST }}{% endif %}\
{% if not is_default_workspace %} {{ BRIGHT_GREEN }}\u{6728}({{ RST }}{{ workspace_name }}{{ BRIGHT_GREEN }}){{ RST }}{% endif %}";

/// Built-in "simple" template — just branch/bookmark, color-coded by dirty state.
///
/// Clean: green branch name. Dirty: yellow branch name.
/// jj: `main` or `xlvlt`  git: `main`
pub const SIMPLE_FORMAT: &str = "\
{% if is_jj %}\
{% if bookmarks and bookmarks[0].distance == 0 %}\
{{ GREEN }}{{ bookmarks[0].name }}{{ RST }}\
{% elif description %}\
{{ GREEN }}{{ description }}{{ RST }}\
{% elif bookmarks and bookmarks[0].distance == 1 %}\
{% if empty %}{{ GREEN }}{% else %}{{ YELLOW }}{% endif %}\
{{ bookmarks[0].name }}{{ RST }}\
{% else %}\
{% if empty %}{{ GREEN }}{% else %}{{ YELLOW }}{% endif %}\
{{ change_id }}{{ RST }}\
{% endif %}\
{% elif is_git %}\
{% if files_changed > 0 %}{{ RED }}\
{% elif staged_files_changed > 0 %}{{ YELLOW }}\
{% else %}{{ GREEN }}\
{% endif %}\
{{ branch }}{{ RST }}\
{% endif %}\
{% if not is_default_workspace %}{{ BRIGHT_GREEN }} \u{6728}{{ RST }}{% endif %}\
{% if conflict %} {{ BRIGHT_RED }}CONFLICT{{ RST }}{% endif %}\
{% if divergent %} {{ BRIGHT_RED }}DIVERGENT{{ RST }}{% endif %}";

/// Built-in "not ready" template for when the daemon hasn't cached status yet.
/// Only color variables are available — no repo status values.
pub const NOT_READY_ASCII: &str = "{{ DIM }}…{{ RST }}";
pub const NOT_READY_NERDFONT: &str = "{{ DIM }}…{{ RST }}";

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
    pub bookmarks: Vec<Bookmark>,
    pub divergent: bool,
    pub hidden: bool,
    pub immutable: bool,

    // git-specific
    pub branch: String,

    // Workspace/worktree
    pub workspace_name: String,
    pub is_default_workspace: bool,
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
            bookmarks: Vec::new(),
            divergent: false,
            hidden: false,
            immutable: false,
            branch: String::new(),
            workspace_name: String::new(),
            is_default_workspace: true,
        }
    }
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
    ctx.insert("bookmarks", &status.bookmarks);
    ctx.insert("has_bookmarks", &!status.bookmarks.is_empty());
    ctx.insert("divergent", &status.divergent);
    ctx.insert("hidden", &status.hidden);
    ctx.insert("immutable", &status.immutable);

    // git-specific
    ctx.insert("branch", &status.branch);
    ctx.insert("has_branch", &!status.branch.is_empty());

    // Workspace/worktree
    ctx.insert("workspace_name", &status.workspace_name);
    ctx.insert("is_default_workspace", &status.is_default_workspace);

    // Color codes — empty strings when color is off
    if color {
        ctx.insert("RST", "\x1b[0m");
        ctx.insert("BOLD", "\x1b[1m");
        ctx.insert("DIM", "\x1b[2m");
        ctx.insert("BLACK", "\x1b[30m");
        ctx.insert("RED", "\x1b[31m");
        ctx.insert("GREEN", "\x1b[32m");
        ctx.insert("YELLOW", "\x1b[33m");
        ctx.insert("BLUE", "\x1b[34m");
        ctx.insert("MAGENTA", "\x1b[35m");
        ctx.insert("CYAN", "\x1b[36m");
        ctx.insert("WHITE", "\x1b[37m");
        ctx.insert("BRIGHT_BLACK", "\x1b[90m");
        ctx.insert("BRIGHT_RED", "\x1b[91m");
        ctx.insert("BRIGHT_GREEN", "\x1b[92m");
        ctx.insert("BRIGHT_YELLOW", "\x1b[93m");
        ctx.insert("BRIGHT_BLUE", "\x1b[94m");
        ctx.insert("BRIGHT_MAGENTA", "\x1b[95m");
        ctx.insert("BRIGHT_CYAN", "\x1b[96m");
        ctx.insert("BRIGHT_WHITE", "\x1b[97m");
    } else {
        let empty = "";
        for name in [
            "RST",
            "BOLD",
            "DIM",
            "BLACK",
            "RED",
            "GREEN",
            "YELLOW",
            "BLUE",
            "MAGENTA",
            "CYAN",
            "WHITE",
            "BRIGHT_BLACK",
            "BRIGHT_RED",
            "BRIGHT_GREEN",
            "BRIGHT_YELLOW",
            "BRIGHT_BLUE",
            "BRIGHT_MAGENTA",
            "BRIGHT_CYAN",
            "BRIGHT_WHITE",
        ] {
            ctx.insert(name, empty);
        }
    }

    match Tera::one_off(template, &ctx, false) {
        Ok(rendered) => rendered.trim().to_string(),
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
            "jj: named workspace",
            RepoStatus {
                is_jj: true,
                change_id: "bfglmprs".into(),
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
    let mut ctx = tera::Context::new();
    if color {
        ctx.insert("RST", "\x1b[0m");
        ctx.insert("BOLD", "\x1b[1m");
        ctx.insert("DIM", "\x1b[2m");
        ctx.insert("BLACK", "\x1b[30m");
        ctx.insert("RED", "\x1b[31m");
        ctx.insert("GREEN", "\x1b[32m");
        ctx.insert("YELLOW", "\x1b[33m");
        ctx.insert("BLUE", "\x1b[34m");
        ctx.insert("MAGENTA", "\x1b[35m");
        ctx.insert("CYAN", "\x1b[36m");
        ctx.insert("WHITE", "\x1b[37m");
        ctx.insert("BRIGHT_BLACK", "\x1b[90m");
        ctx.insert("BRIGHT_RED", "\x1b[91m");
        ctx.insert("BRIGHT_GREEN", "\x1b[92m");
        ctx.insert("BRIGHT_YELLOW", "\x1b[93m");
        ctx.insert("BRIGHT_BLUE", "\x1b[94m");
        ctx.insert("BRIGHT_MAGENTA", "\x1b[95m");
        ctx.insert("BRIGHT_CYAN", "\x1b[96m");
        ctx.insert("BRIGHT_WHITE", "\x1b[97m");
    } else {
        let empty = "";
        for name in [
            "RST",
            "BOLD",
            "DIM",
            "BLACK",
            "RED",
            "GREEN",
            "YELLOW",
            "BLUE",
            "MAGENTA",
            "CYAN",
            "WHITE",
            "BRIGHT_BLACK",
            "BRIGHT_RED",
            "BRIGHT_GREEN",
            "BRIGHT_YELLOW",
            "BRIGHT_BLUE",
            "BRIGHT_MAGENTA",
            "BRIGHT_CYAN",
            "BRIGHT_WHITE",
        ] {
            ctx.insert(name, empty);
        }
    }
    match Tera::one_off(template, &ctx, false) {
        Ok(rendered) => rendered.trim().to_string(),
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
{% if is_jj %}{{ MAGENTA }}JJ{{ RST }} {{ change_id }}
{%- for b in bookmarks %} {{ BLUE }}{{ b.display }}{{ RST }}{% endfor %}
{%- elif is_git %}{{ GREEN }}+{{ RED }}-{{ RST }} {{ BLUE }}{{ branch }}{{ RST }} {{ commit_id }}
{%- endif %}
{%- if total_files_changed > 0 %} {{ BLUE }}[{{ RST }}{{ BRIGHT_BLUE }}{{ total_files_changed }}{{ RST }} {{ BRIGHT_GREEN }}+{{ total_lines_added }}{{ RST }}{{ BRIGHT_RED }}-{{ total_lines_removed }}{{ RST }}{{ BLUE }}]{{ RST }}{% endif %}
{%- if conflict %} {{ BRIGHT_RED }}CONFLICT{{ RST }}{% endif %}
{%- if divergent %} {{ BRIGHT_RED }}DIVERGENT{{ RST }}{% endif %}
{%- if hidden %} {{ BRIGHT_YELLOW }}HIDDEN{{ RST }}{% endif %}
{%- if immutable %} {{ YELLOW }}IMMUTABLE{{ RST }}{% endif %}
{%- if empty %} {{ BLUE }}({{ RST }}EMPTY{{ BLUE }}){{ RST }}{% endif %}
{%- if not is_default_workspace %} {{ BRIGHT_GREEN }}/\({{ RST }}{{ workspace_name }}{{ BRIGHT_GREEN }}/\{{ RST }}{% endif %}'''
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
            formatted.contains("\u{203B}"),
            "expected reference mark: {formatted:?}"
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
        let tmpl = "{{ YELLOW }}loading{{ RST }}";
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
}
