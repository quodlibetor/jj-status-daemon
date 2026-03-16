use anyhow::{Context, Result};
use std::path::Path;
use tokio::process::Command;

use crate::config::Config;
use crate::jj::{RepoStatus, parse_diff_stat};

async fn run_git(repo_path: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo_path)
        .output()
        .await
        .context("failed to run git")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git command failed: {stderr}");
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

pub async fn query_git_status(repo_path: &Path, _config: &Config) -> Result<RepoStatus> {
    let mut status = RepoStatus {
        is_git: true,
        ..Default::default()
    };

    let branch_fut = async {
        let branch = run_git(repo_path, &["symbolic-ref", "--short", "HEAD"]).await;
        match branch {
            Ok(b) => Ok(b.trim().to_string()),
            Err(_) => {
                // Detached HEAD — fall back to short commit id
                run_git(repo_path, &["rev-parse", "--short", "HEAD"])
                    .await
                    .map(|s| s.trim().to_string())
            }
        }
    };

    let commit_fut = async {
        run_git(repo_path, &["rev-parse", "--short", "HEAD"])
            .await
            .map(|s| s.trim().to_string())
    };

    let description_fut = async {
        run_git(repo_path, &["log", "-1", "--format=%s"])
            .await
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    };

    // Unstaged: working tree vs index
    let unstaged_fut = async {
        run_git(repo_path, &["diff", "--stat"])
            .await
            .unwrap_or_default()
    };

    // Staged: index vs HEAD
    let staged_fut = async {
        run_git(repo_path, &["diff", "--cached", "--stat"])
            .await
            .unwrap_or_default()
    };

    // Total: working tree vs HEAD
    let total_fut = async {
        run_git(repo_path, &["diff", "--stat", "HEAD"])
            .await
            .unwrap_or_default()
    };

    let empty_fut = async {
        // --quiet exits 0 if no diff (empty), 1 if diff exists
        run_git(repo_path, &["diff", "--quiet", "HEAD~1", "HEAD"])
            .await
            .is_ok()
    };

    let conflict_fut = async {
        let result = run_git(repo_path, &["diff", "--name-only", "--diff-filter=U"]).await;
        match result {
            Ok(output) => !output.trim().is_empty(),
            Err(_) => false,
        }
    };

    let (branch, commit, description, unstaged_out, staged_out, total_out, empty, conflict) = tokio::join!(
        branch_fut,
        commit_fut,
        description_fut,
        unstaged_fut,
        staged_fut,
        total_fut,
        empty_fut,
        conflict_fut
    );

    status.branch = branch.unwrap_or_default();
    status.commit_id = commit.unwrap_or_default();
    status.description = description;
    status.empty = empty;
    status.conflict = conflict;

    let (f, a, r) = parse_diff_stat(&unstaged_out);
    status.files_changed = f;
    status.lines_added = a;
    status.lines_removed = r;

    let (f, a, r) = parse_diff_stat(&staged_out);
    status.staged_files_changed = f;
    status.staged_lines_added = a;
    status.staged_lines_removed = r;

    let (f, a, r) = parse_diff_stat(&total_out);
    status.total_files_changed = f;
    status.total_lines_added = a;
    status.total_lines_removed = r;

    Ok(status)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    async fn create_git_repo() -> TempDir {
        let dir = TempDir::new().unwrap();
        let run = |args: &[&str]| {
            let dir_path = dir.path().to_path_buf();
            let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
            async move {
                let output = Command::new("git")
                    .args(&args)
                    .current_dir(&dir_path)
                    .output()
                    .await
                    .unwrap();
                assert!(
                    output.status.success(),
                    "git {:?} failed: {}",
                    args,
                    String::from_utf8_lossy(&output.stderr)
                );
            }
        };
        run(&["init"]).await;
        run(&["config", "user.email", "test@test.com"]).await;
        run(&["config", "user.name", "Test"]).await;
        // Create an initial commit so HEAD exists
        std::fs::write(dir.path().join("README"), "init\n").unwrap();
        run(&["add", "."]).await;
        run(&["commit", "-m", "initial"]).await;
        dir
    }

    async fn git_cmd(repo: &std::path::Path, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(repo)
            .output()
            .await
            .unwrap();
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[tokio::test]
    async fn test_git_basic_status() {
        let dir = create_git_repo().await;
        let config = Config {
            color: false,
            ..Default::default()
        };
        let status = query_git_status(dir.path(), &config).await.unwrap();
        assert!(status.is_git);
        assert!(!status.is_jj);
        assert!(!status.commit_id.is_empty());
        assert!(!status.branch.is_empty());
        assert_eq!(status.description, "initial");
    }

    #[tokio::test]
    async fn test_git_branch_name() {
        let dir = create_git_repo().await;
        let config = Config {
            color: false,
            ..Default::default()
        };
        let status = query_git_status(dir.path(), &config).await.unwrap();
        assert!(
            status.branch == "main" || status.branch == "master",
            "expected main or master, got: {:?}",
            status.branch
        );
    }

    #[tokio::test]
    async fn test_git_description() {
        let dir = create_git_repo().await;
        git_cmd(
            dir.path(),
            &["commit", "--allow-empty", "-m", "my cool feature"],
        )
        .await;
        let config = Config {
            color: false,
            ..Default::default()
        };
        let status = query_git_status(dir.path(), &config).await.unwrap();
        assert_eq!(status.description, "my cool feature");
    }

    #[tokio::test]
    async fn test_git_unstaged_changes() {
        let dir = create_git_repo().await;
        // Modify a tracked file without staging
        std::fs::write(dir.path().join("README"), "init\nhello\nworld\n").unwrap();
        let config = Config {
            color: false,
            ..Default::default()
        };
        let status = query_git_status(dir.path(), &config).await.unwrap();
        // Unstaged: working tree vs index — should show the change
        assert!(
            status.files_changed >= 1,
            "expected unstaged files_changed >= 1, got {}",
            status.files_changed
        );
        assert!(
            status.lines_added > 0,
            "expected unstaged lines_added > 0, got {}",
            status.lines_added
        );
        // Staged: nothing staged
        assert_eq!(status.staged_files_changed, 0);
        // Total: same as unstaged since nothing is staged
        assert_eq!(status.total_files_changed, status.files_changed);
        assert_eq!(status.total_lines_added, status.lines_added);
    }

    #[tokio::test]
    async fn test_git_staged_changes() {
        let dir = create_git_repo().await;
        // Modify and stage a file
        std::fs::write(dir.path().join("README"), "init\nstaged line\n").unwrap();
        git_cmd(dir.path(), &["add", "README"]).await;
        let config = Config {
            color: false,
            ..Default::default()
        };
        let status = query_git_status(dir.path(), &config).await.unwrap();
        // Unstaged: nothing unstaged (change is in index)
        assert_eq!(
            status.files_changed, 0,
            "expected no unstaged changes, got files_changed={}",
            status.files_changed
        );
        // Staged: should show the change
        assert!(
            status.staged_files_changed >= 1,
            "expected staged_files_changed >= 1, got {}",
            status.staged_files_changed
        );
        assert!(
            status.staged_lines_added > 0,
            "expected staged_lines_added > 0, got {}",
            status.staged_lines_added
        );
        // Total: same as staged
        assert_eq!(status.total_files_changed, status.staged_files_changed);
    }

    #[tokio::test]
    async fn test_git_mixed_staged_unstaged() {
        let dir = create_git_repo().await;
        // Stage a change to README
        std::fs::write(dir.path().join("README"), "init\nstaged\n").unwrap();
        git_cmd(dir.path(), &["add", "README"]).await;
        // Then make a further unstaged change
        std::fs::write(dir.path().join("README"), "init\nstaged\nunstaged\n").unwrap();
        let config = Config {
            color: false,
            ..Default::default()
        };
        let status = query_git_status(dir.path(), &config).await.unwrap();
        assert!(
            status.files_changed >= 1,
            "expected unstaged files_changed >= 1, got {}",
            status.files_changed
        );
        assert!(
            status.staged_files_changed >= 1,
            "expected staged_files_changed >= 1, got {}",
            status.staged_files_changed
        );
        assert!(
            status.total_files_changed >= 1,
            "expected total_files_changed >= 1, got {}",
            status.total_files_changed
        );
        // Total lines should be >= staged + unstaged (though file counts may not add)
        assert!(
            status.total_lines_added >= status.staged_lines_added,
            "total_lines_added ({}) should be >= staged_lines_added ({})",
            status.total_lines_added,
            status.staged_lines_added
        );
    }

    #[tokio::test]
    async fn test_git_empty_commit() {
        let dir = create_git_repo().await;
        Command::new("git")
            .args(["commit", "--allow-empty", "-m", "empty"])
            .current_dir(dir.path())
            .output()
            .await
            .unwrap();
        let config = Config {
            color: false,
            ..Default::default()
        };
        let status = query_git_status(dir.path(), &config).await.unwrap();
        assert!(status.empty, "expected empty commit to be detected");
    }

    #[tokio::test]
    async fn test_git_format_with_branch() {
        use crate::jj::format_status;
        let dir = create_git_repo().await;
        let config = Config {
            color: false,
            ..Default::default()
        };
        let status = query_git_status(dir.path(), &config).await.unwrap();
        let template = "{% if is_git %}{{ branch }}{% endif %} {{ commit_id }} {{ description }}";
        let formatted = format_status(&status, template, false);
        assert!(
            formatted.contains(&status.branch),
            "expected branch in output: {formatted:?}"
        );
        assert!(
            formatted.contains(&status.commit_id),
            "expected commit_id in output: {formatted:?}"
        );
        assert!(
            formatted.contains("initial"),
            "expected description in output: {formatted:?}"
        );
    }
}
