use anyhow::{Context, Result};
use std::path::Path;
use tokio::process::Command;

use crate::config::Config;

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
    pub bookmarks: Vec<(String, u32)>, // (name, distance from @)
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

    let commit_fut = async {
        let mut args = vec!["log", "-r", "@", "--no-graph", "-R", &repo_str];
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
        status.change_id = parts[0].to_string();
        status.commit_id = parts[1].to_string();
        status.description = parts[2].to_string();
        status.conflict = parts[3] == "true";
        status.divergent = parts[4] == "true";
        status.hidden = parts[5] == "true";
        status.immutable = parts[6] == "true";
        status.empty = parts[7] == "true";
    }

    // Parse bookmarks - each line corresponds to an ancestor at distance i
    // Empty lines are significant (ancestor with no bookmarks), so don't skip them
    for (distance, line) in bookmark_out.lines().enumerate() {
        for bookmark in line.split_whitespace() {
            if !bookmark.is_empty() {
                status.bookmarks.push((bookmark.to_string(), distance as u32));
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

pub fn format_status(status: &JjStatus, format: &str) -> String {
    let bookmarks_str = if status.bookmarks.is_empty() {
        String::new()
    } else {
        status
            .bookmarks
            .iter()
            .map(|(name, dist)| {
                if *dist == 0 {
                    name.clone()
                } else {
                    format!("{name}+{dist}")
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    };

    let mut state_parts = Vec::new();
    if status.conflict {
        state_parts.push("CONFLICT");
    }
    if status.divergent {
        state_parts.push("DIVERGENT");
    }
    if status.hidden {
        state_parts.push("HIDDEN");
    }
    if status.immutable {
        state_parts.push("IMMUTABLE");
    }
    if status.empty {
        state_parts.push("EMPTY");
    }
    let state_str = if state_parts.is_empty() {
        String::new()
    } else {
        format!("({})", state_parts.join(" "))
    };

    let metrics_str = if status.files_changed > 0 {
        format!(
            "[{} +{}-{}]",
            status.files_changed, status.lines_added, status.lines_removed
        )
    } else {
        String::new()
    };

    let result = format
        .replace("{change_id}", &status.change_id)
        .replace("{commit_id}", &status.commit_id)
        .replace("{description}", &status.description)
        .replace("{bookmarks}", &bookmarks_str)
        .replace("{state}", &state_str)
        .replace("{metrics}", &metrics_str);

    // Clean up extra whitespace from empty replacements
    let result = result
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");

    result.trim().to_string()
}

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
        let config = Config::default();
        let status = query_jj_status(dir.path(), &config, false).await.unwrap();
        assert!(!status.change_id.is_empty());
        assert!(status.empty);
        assert!(status.bookmarks.is_empty());
    }

    #[tokio::test]
    async fn test_with_description() {
        let dir = create_jj_repo().await;
        jj_cmd(dir.path(), &["describe", "-m", "hello world"]).await;
        let config = Config::default();
        let status = query_jj_status(dir.path(), &config, false).await.unwrap();
        assert_eq!(status.description, "hello world");
    }

    #[tokio::test]
    async fn test_with_bookmark() {
        let dir = create_jj_repo().await;
        jj_cmd(dir.path(), &["bookmark", "create", "main", "-r", "@"]).await;
        let config = Config::default();
        let status = query_jj_status(dir.path(), &config, false).await.unwrap();
        assert!(status.bookmarks.iter().any(|(name, dist)| name == "main" && *dist == 0));
    }

    #[tokio::test]
    async fn test_bookmark_distance() {
        let dir = create_jj_repo().await;
        jj_cmd(dir.path(), &["bookmark", "create", "main", "-r", "@"]).await;
        jj_cmd(dir.path(), &["new"]).await;
        let config = Config::default();
        let status = query_jj_status(dir.path(), &config, false).await.unwrap();
        assert!(status.bookmarks.iter().any(|(name, dist)| name == "main" && *dist == 1));
    }

    #[tokio::test]
    async fn test_diff_stats() {
        let dir = create_jj_repo().await;
        std::fs::write(dir.path().join("test.txt"), "hello\nworld\n").unwrap();
        // jj will auto-snapshot
        let config = Config::default();
        let status = query_jj_status(dir.path(), &config, false).await.unwrap();
        assert!(status.files_changed >= 1);
        assert!(status.lines_added > 0);
    }

    #[test]
    fn test_format_status() {
        let status = JjStatus {
            change_id: "mrtu".to_string(),
            commit_id: "abc1".to_string(),
            description: "test".to_string(),
            empty: false,
            bookmarks: vec![("main".to_string(), 0)],
            files_changed: 3,
            lines_added: 10,
            lines_removed: 5,
            ..Default::default()
        };
        let formatted = format_status(&status, "{change_id} {bookmarks}{metrics} {state}");
        assert_eq!(formatted, "mrtu main[3 +10-5]");
    }

    #[test]
    fn test_format_status_empty() {
        let status = JjStatus {
            change_id: "mrtu".to_string(),
            commit_id: "abc1".to_string(),
            empty: true,
            ..Default::default()
        };
        let formatted = format_status(&status, "{change_id} {bookmarks}{metrics} {state}");
        assert_eq!(formatted, "mrtu (EMPTY)");
    }
}
