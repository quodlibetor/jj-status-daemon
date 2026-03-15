use anyhow::{Context, Result};
use serde::Serialize;
use std::path::Path;
use tera::Tera;
use tokio::process::Command;

use crate::config::Config;

#[derive(Debug, Clone, Default, Serialize)]
pub struct Bookmark {
    pub name: String,
    pub distance: u32,
    /// Pre-formatted display string: "main" or "main+2"
    pub display: String,
}

#[derive(Debug, Clone, Default)]
pub struct JjStatus {
    pub change_id: String,
    pub commit_id: String,
    pub description: String,
    pub conflict: bool,
    pub divergent: bool,
    pub hidden: bool,
    pub immutable: bool,
    pub empty: bool,
    pub bookmarks: Vec<Bookmark>,
    pub files_changed: u32,
    pub lines_added: u32,
    pub lines_removed: u32,
}

async fn run_jj(repo_path: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("jj")
        .args(args)
        .current_dir(repo_path)
        .output()
        .await
        .context("failed to run jj")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("jj command failed: {stderr}");
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Strip ANSI escape sequences from a string.
fn strip_ansi(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            while let Some(&next) = chars.peek() {
                chars.next();
                if next.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            result.push(c);
        }
    }
    result
}

pub async fn query_jj_status(repo_path: &Path, config: &Config, ignore_working_copy: bool) -> Result<JjStatus> {
    let iwc: &[&str] = if ignore_working_copy {
        &["--ignore-working-copy"]
    } else {
        &[]
    };

    let repo_str = repo_path.to_string_lossy().to_string();

    let commit_template = r#"change_id.shortest(8) ++ "|||" ++ commit_id.shortest(8) ++ "|||" ++ description.first_line() ++ "|||" ++ conflict ++ "|||" ++ divergent ++ "|||" ++ hidden ++ "|||" ++ immutable ++ "|||" ++ empty"#;

    let bookmark_template = r#"bookmarks.map(|b| b.name()).join(" ") ++ "\n""#;
    let depth = config.bookmark_search_depth;

    let repo_str2 = repo_str.clone();
    let repo_str3 = repo_str.clone();

    let iwc_owned: Vec<String> = iwc.iter().map(|s| s.to_string()).collect();
    let iwc_owned2 = iwc_owned.clone();
    let iwc_owned3 = iwc_owned.clone();

    let color_flag = if config.color { "--color=always" } else { "--color=never" };

    let commit_fut = async {
        let mut args = vec!["log", "-r", "@", "--no-graph", color_flag, "-R", &repo_str];
        for a in &iwc_owned {
            args.push(a);
        }
        args.extend_from_slice(&["-T", commit_template]);
        run_jj(repo_path, &args).await
    };

    let bookmark_fut = async {
        let ancestor_expr = format!("ancestors(@, {depth})");
        let mut args = vec!["log", "-r", &ancestor_expr, "--no-graph", "-R", &repo_str2];
        for a in &iwc_owned2 {
            args.push(a);
        }
        args.extend_from_slice(&["-T", bookmark_template]);
        run_jj(repo_path, &args).await
    };

    let diff_fut = async {
        let mut args = vec!["diff", "-r", "@", "--stat", "-R", &repo_str3];
        for a in &iwc_owned3 {
            args.push(a);
        }
        run_jj(repo_path, &args).await
    };

    let (commit_out, bookmark_out, diff_out) = tokio::try_join!(commit_fut, bookmark_fut, diff_fut)?;

    let mut status = JjStatus::default();

    // Parse commit info
    let commit_line = commit_out.trim();
    let parts: Vec<&str> = commit_line.split("|||").collect();
    if parts.len() >= 8 {
        // change_id and commit_id keep their ANSI colors (from jj)
        status.change_id = parts[0].to_string();
        status.commit_id = parts[1].to_string();
        // Strip ANSI from fields we parse as text/booleans
        status.description = strip_ansi(parts[2]);
        status.conflict = strip_ansi(parts[3]) == "true";
        status.divergent = strip_ansi(parts[4]) == "true";
        status.hidden = strip_ansi(parts[5]) == "true";
        status.immutable = strip_ansi(parts[6]) == "true";
        status.empty = strip_ansi(parts[7]) == "true";
    }

    // Parse bookmarks - each line corresponds to an ancestor at distance i
    // Empty lines are significant (ancestor with no bookmarks), so don't skip them
    for (distance, line) in bookmark_out.lines().enumerate() {
        let dist = distance as u32;
        for name in line.split_whitespace() {
            if !name.is_empty() {
                let display = if dist == 0 {
                    name.to_string()
                } else {
                    format!("{name}+{dist}")
                };
                status.bookmarks.push(Bookmark {
                    name: name.to_string(),
                    distance: dist,
                    display,
                });
            }
        }
    }

    // Parse diff stats - look for summary line like "3 files changed, 10 insertions(+), 5 deletions(-)"
    for line in diff_out.lines() {
        let line = line.trim();
        if line.contains("changed") {
            // Parse "N file(s) changed"
            if let Some(n) = line.split_whitespace().next() {
                status.files_changed = n.parse().unwrap_or(0);
            }
            // Parse "N insertions(+)"
            if let Some(idx) = line.find("insertion") {
                let before = &line[..idx].trim();
                if let Some(n) = before.rsplit(", ").next().or(before.rsplit(' ').next()) {
                    status.lines_added = n.trim().parse().unwrap_or(0);
                }
            }
            // Parse "N deletions(-)"
            if let Some(idx) = line.find("deletion") {
                let before = &line[..idx].trim();
                if let Some(n) = before.rsplit(", ").next().or(before.rsplit(' ').next()) {
                    status.lines_removed = n.trim().parse().unwrap_or(0);
                }
            }
        }
    }

    Ok(status)
}

pub fn format_status(status: &JjStatus, template: &str, color: bool) -> String {
    let mut ctx = tera::Context::new();

    // Data variables
    ctx.insert("change_id", &status.change_id);
    ctx.insert("commit_id", &status.commit_id);
    ctx.insert("description", &status.description);
    ctx.insert("conflict", &status.conflict);
    ctx.insert("divergent", &status.divergent);
    ctx.insert("hidden", &status.hidden);
    ctx.insert("immutable", &status.immutable);
    ctx.insert("empty", &status.empty);
    ctx.insert("files_changed", &status.files_changed);
    ctx.insert("lines_added", &status.lines_added);
    ctx.insert("lines_removed", &status.lines_removed);

    // Bookmarks as a list of objects with name, distance, display
    ctx.insert("bookmarks", &status.bookmarks);
    ctx.insert("has_bookmarks", &!status.bookmarks.is_empty());

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
            "RST", "BOLD", "DIM",
            "BLACK", "RED", "GREEN", "YELLOW", "BLUE", "MAGENTA", "CYAN", "WHITE",
            "BRIGHT_BLACK", "BRIGHT_RED", "BRIGHT_GREEN", "BRIGHT_YELLOW",
            "BRIGHT_BLUE", "BRIGHT_MAGENTA", "BRIGHT_CYAN", "BRIGHT_WHITE",
        ] {
            ctx.insert(name, empty);
        }
    }

    match Tera::one_off(template, &ctx, false) {
        Ok(rendered) => rendered.trim().to_string(),
        Err(e) => format!("template error: {e}"),
    }
}

/// Default Tera template for status formatting.
pub const DEFAULT_FORMAT: &str = "\
{{ change_id }}\
{% for b in bookmarks %} {{ BLUE }}{{ b.display }}{{ RST }}{% endfor %}\
{% if files_changed > 0 %} {{ BLUE }}[{{ RST }}\
{{ BRIGHT_BLUE }}{{ files_changed }}{{ RST }} \
{{ BRIGHT_GREEN }}+{{ lines_added }}{{ RST }}\
{{ BRIGHT_RED }}-{{ lines_removed }}{{ RST }}\
{{ BLUE }}]{{ RST }}{% endif %}\
{% if conflict %} {{ BRIGHT_RED }}CONFLICT{{ RST }}{% endif %}\
{% if divergent %} {{ BRIGHT_RED }}DIVERGENT{{ RST }}{% endif %}\
{% if hidden %} {{ BRIGHT_YELLOW }}HIDDEN{{ RST }}{% endif %}\
{% if immutable %} {{ YELLOW }}IMMUTABLE{{ RST }}{% endif %}\
{% if empty %} {{ BLUE }}({{ RST }}EMPTY{{ BLUE }}){{ RST }}{% endif %}";

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    async fn create_jj_repo() -> TempDir {
        let dir = TempDir::new().unwrap();
        let output = Command::new("jj")
            .args(["git", "init"])
            .current_dir(dir.path())
            .output()
            .await
            .unwrap();
        assert!(output.status.success(), "jj git init failed: {}", String::from_utf8_lossy(&output.stderr));
        dir
    }

    async fn jj_cmd(repo: &Path, args: &[&str]) -> String {
        let output = Command::new("jj")
            .args(args)
            .current_dir(repo)
            .output()
            .await
            .unwrap();
        assert!(output.status.success(), "jj {:?} failed: {}", args, String::from_utf8_lossy(&output.stderr));
        String::from_utf8_lossy(&output.stdout).to_string()
    }

    #[tokio::test]
    async fn test_empty_repo() {
        let dir = create_jj_repo().await;
        let config = Config { color: false, ..Default::default() };
        let status = query_jj_status(dir.path(), &config, false).await.unwrap();
        assert!(!status.change_id.is_empty());
        assert!(status.empty);
        assert!(status.bookmarks.is_empty());
    }

    #[tokio::test]
    async fn test_with_description() {
        let dir = create_jj_repo().await;
        jj_cmd(dir.path(), &["describe", "-m", "hello world"]).await;
        let config = Config { color: false, ..Default::default() };
        let status = query_jj_status(dir.path(), &config, false).await.unwrap();
        assert_eq!(status.description, "hello world");
    }

    #[tokio::test]
    async fn test_with_bookmark() {
        let dir = create_jj_repo().await;
        jj_cmd(dir.path(), &["bookmark", "create", "main", "-r", "@"]).await;
        let config = Config { color: false, ..Default::default() };
        let status = query_jj_status(dir.path(), &config, false).await.unwrap();
        assert!(status.bookmarks.iter().any(|b| b.name == "main" && b.distance == 0 && b.display == "main"));
    }

    #[tokio::test]
    async fn test_bookmark_distance() {
        let dir = create_jj_repo().await;
        jj_cmd(dir.path(), &["bookmark", "create", "main", "-r", "@"]).await;
        jj_cmd(dir.path(), &["new"]).await;
        let config = Config { color: false, ..Default::default() };
        let status = query_jj_status(dir.path(), &config, false).await.unwrap();
        assert!(status.bookmarks.iter().any(|b| b.name == "main" && b.distance == 1 && b.display == "main+1"));
    }

    #[tokio::test]
    async fn test_diff_stats() {
        let dir = create_jj_repo().await;
        std::fs::write(dir.path().join("test.txt"), "hello\nworld\n").unwrap();
        let config = Config { color: false, ..Default::default() };
        let status = query_jj_status(dir.path(), &config, false).await.unwrap();
        assert!(status.files_changed >= 1);
        assert!(status.lines_added > 0);
    }

    #[test]
    fn test_format_status_with_metrics() {
        let status = JjStatus {
            change_id: "mrtu".to_string(),
            commit_id: "abc1".to_string(),
            description: "test".to_string(),
            empty: false,
            bookmarks: vec![Bookmark { name: "main".into(), distance: 0, display: "main".into() }],
            files_changed: 3,
            lines_added: 10,
            lines_removed: 5,
            ..Default::default()
        };
        let formatted = format_status(&status, DEFAULT_FORMAT, false);
        assert_eq!(formatted, "mrtu main [3 +10-5]");
    }

    #[test]
    fn test_format_status_empty() {
        let status = JjStatus {
            change_id: "mrtu".to_string(),
            commit_id: "abc1".to_string(),
            empty: true,
            ..Default::default()
        };
        let formatted = format_status(&status, DEFAULT_FORMAT, false);
        assert_eq!(formatted, "mrtu (EMPTY)");
    }

    #[test]
    fn test_format_custom_template() {
        let status = JjStatus {
            change_id: "mrtu".to_string(),
            commit_id: "abc1".to_string(),
            description: "my change".to_string(),
            bookmarks: vec![Bookmark { name: "main".into(), distance: 0, display: "main".into() }],
            ..Default::default()
        };
        let tmpl = "{{ commit_id }}:{{ change_id }} {{ description }}";
        let formatted = format_status(&status, tmpl, false);
        assert_eq!(formatted, "abc1:mrtu my change");
    }

    #[test]
    fn test_format_toml_multiline_matches_default() {
        // This is the multi-line TOML format from the README. It must produce
        // identical output to DEFAULT_FORMAT for every combination of fields.
        let toml_str = r#"
format = '''
{{ change_id }}
{%- for b in bookmarks %} {{ BLUE }}{{ b.display }}{{ RST }}{% endfor %}
{%- if files_changed > 0 %} {{ BLUE }}[{{ RST }}{{ BRIGHT_BLUE }}{{ files_changed }}{{ RST }} {{ BRIGHT_GREEN }}+{{ lines_added }}{{ RST }}{{ BRIGHT_RED }}-{{ lines_removed }}{{ RST }}{{ BLUE }}]{{ RST }}{% endif %}
{%- if conflict %} {{ BRIGHT_RED }}CONFLICT{{ RST }}{% endif %}
{%- if divergent %} {{ BRIGHT_RED }}DIVERGENT{{ RST }}{% endif %}
{%- if hidden %} {{ BRIGHT_YELLOW }}HIDDEN{{ RST }}{% endif %}
{%- if immutable %} {{ YELLOW }}IMMUTABLE{{ RST }}{% endif %}
{%- if empty %} {{ BLUE }}({{ RST }}EMPTY{{ BLUE }}){{ RST }}{% endif %}'''
"#;
        let config: Config = toml::from_str(toml_str).unwrap();

        let cases = vec![
            // Empty repo
            JjStatus {
                change_id: "mrtu".into(),
                empty: true,
                ..Default::default()
            },
            // With bookmarks and metrics
            JjStatus {
                change_id: "mrtu".into(),
                bookmarks: vec![Bookmark { name: "main".into(), distance: 0, display: "main".into() }],
                files_changed: 3,
                lines_added: 10,
                lines_removed: 5,
                ..Default::default()
            },
            // Multiple bookmarks at different distances
            JjStatus {
                change_id: "abcd".into(),
                bookmarks: vec![
                    Bookmark { name: "feat".into(), distance: 0, display: "feat".into() },
                    Bookmark { name: "main".into(), distance: 2, display: "main+2".into() },
                ],
                files_changed: 1,
                lines_added: 7,
                lines_removed: 0,
                ..Default::default()
            },
            // Conflict state
            JjStatus {
                change_id: "zzzz".into(),
                conflict: true,
                empty: true,
                ..Default::default()
            },
        ];

        for (i, status) in cases.iter().enumerate() {
            let from_default = format_status(status, DEFAULT_FORMAT, false);
            let from_toml = format_status(status, &config.format, false);
            assert_eq!(
                from_default, from_toml,
                "case {i}: DEFAULT_FORMAT and TOML multi-line produced different output\n  default: {from_default:?}\n  toml:    {from_toml:?}"
            );
        }
    }

    #[test]
    fn test_format_conflict_state() {
        let status = JjStatus {
            change_id: "mrtu".to_string(),
            conflict: true,
            ..Default::default()
        };
        let formatted = format_status(&status, DEFAULT_FORMAT, false);
        assert!(formatted.contains("CONFLICT"));
    }
}
