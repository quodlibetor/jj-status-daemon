use anyhow::{Context, Result};
use std::path::Path;
use std::time::Duration;

use crate::config::Config;
use crate::template::RepoStatus;

const GIT2_TIMEOUT: Duration = Duration::from_secs(30);

fn diff_stats(diff: &git2::Diff<'_>) -> Result<(u32, u32, u32)> {
    let stats = diff.stats()?;
    Ok((
        stats.files_changed() as u32,
        stats.insertions() as u32,
        stats.deletions() as u32,
    ))
}

#[tracing::instrument(fields(repo = %repo_path.display()))]
fn query_git_status_blocking(repo_path: &Path) -> Result<RepoStatus> {
    let repo = {
        let _span = tracing::debug_span!("git_open").entered();
        git2::Repository::open(repo_path).context("failed to open git repo")?
    };

    let mut status = RepoStatus {
        is_git: true,
        ..Default::default()
    };

    // Branch and commit info
    let head = match repo.head() {
        Ok(head) => head,
        Err(_) => {
            // Unborn HEAD (empty repo)
            return Ok(status);
        }
    };

    // Branch name (or short OID if detached)
    status.branch = head.shorthand().unwrap_or("").to_string();

    // Commit ID (short), description, and tree for diff stats
    let head_tree = if let Ok(commit) = head.peel_to_commit() {
        let oid = commit.id();
        status.commit_id = commit
            .as_object()
            .short_id()
            .map(|buf| buf.as_str().unwrap_or("").to_string())
            .unwrap_or_else(|_| format!("{:.7}", oid));
        status.description = commit.summary().unwrap_or("").to_string();

        // Empty detection: compare HEAD tree to parent tree
        let head_tree = commit.tree().ok();
        let parent_tree = commit.parent(0).ok().and_then(|p| p.tree().ok());
        if let Some(head_tree) = &head_tree {
            let diff = repo.diff_tree_to_tree(parent_tree.as_ref(), Some(head_tree), None);
            status.empty = match diff {
                Ok(d) => d.stats().map(|s| s.files_changed() == 0).unwrap_or(false),
                Err(_) => false,
            };
        }
        head_tree
    } else {
        None
    };

    // Conflict detection
    status.conflict = repo.index().map(|idx| idx.has_conflicts()).unwrap_or(false);

    // Unstaged: index → workdir
    {
        let _span = tracing::debug_span!("diff_unstaged").entered();
        let mut diff_opts = git2::DiffOptions::new();
        if let Ok(diff) = repo.diff_index_to_workdir(None, Some(&mut diff_opts)) {
            let (f, a, r) = diff_stats(&diff)?;
            status.files_changed = f;
            status.lines_added = a;
            status.lines_removed = r;
        }
    }

    // Staged: tree → index
    {
        let _span = tracing::debug_span!("diff_staged").entered();
        if let Ok(diff) = repo.diff_tree_to_index(head_tree.as_ref(), None, None) {
            let (f, a, r) = diff_stats(&diff)?;
            status.staged_files_changed = f;
            status.staged_lines_added = a;
            status.staged_lines_removed = r;
        }
    }

    // Total: tree → workdir (with index)
    {
        let _span = tracing::debug_span!("diff_total").entered();
        if let Ok(diff) = repo.diff_tree_to_workdir_with_index(head_tree.as_ref(), None) {
            let (f, a, r) = diff_stats(&diff)?;
            status.total_files_changed = f;
            status.total_lines_added = a;
            status.total_lines_removed = r;
        }
    }

    // Worktree detection
    if repo.is_worktree() {
        status.workspace_name = repo_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "worktree".to_string());
        status.is_default_workspace = false;
    } else {
        status.workspace_name = "main".to_string();
        status.is_default_workspace = true;
    }

    // Rebase detection
    status.rebasing = matches!(
        repo.state(),
        git2::RepositoryState::Rebase
            | git2::RepositoryState::RebaseInteractive
            | git2::RepositoryState::RebaseMerge
            | git2::RepositoryState::ApplyMailbox
            | git2::RepositoryState::ApplyMailboxOrRebase
    );

    Ok(status)
}

#[tracing::instrument(skip(_config), fields(repo = %repo_path.display()))]
pub async fn query_git_status(repo_path: &Path, _config: &Config) -> Result<RepoStatus> {
    let repo_path = repo_path.to_path_buf();
    tokio::time::timeout(
        GIT2_TIMEOUT,
        tokio::task::spawn_blocking(move || query_git_status_blocking(&repo_path)),
    )
    .await
    .context("git2 query timed out")?
    .context("git2 task panicked")?
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use tokio::process::Command;

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
    async fn test_git_main_worktree() {
        let dir = create_git_repo().await;
        let config = Config {
            color: false,
            ..Default::default()
        };
        let status = query_git_status(dir.path(), &config).await.unwrap();
        assert_eq!(status.workspace_name, "main");
        assert!(status.is_default_workspace);
    }

    #[tokio::test]
    async fn test_git_linked_worktree() {
        let dir = create_git_repo().await;
        let wt_dir = TempDir::with_prefix("git-wt-").unwrap();
        let wt_path = wt_dir.path().join("my-feature");
        git_cmd(
            dir.path(),
            &[
                "worktree",
                "add",
                wt_path.to_str().unwrap(),
                "-b",
                "feature",
            ],
        )
        .await;

        let config = Config {
            color: false,
            ..Default::default()
        };
        let status = query_git_status(&wt_path, &config).await.unwrap();
        assert_eq!(status.workspace_name, "my-feature");
        assert!(!status.is_default_workspace);
        assert_eq!(status.branch, "feature");
    }

    #[tokio::test]
    async fn test_git_not_rebasing() {
        let dir = create_git_repo().await;
        let config = Config {
            color: false,
            ..Default::default()
        };
        let status = query_git_status(dir.path(), &config).await.unwrap();
        assert!(!status.rebasing);
    }

    #[tokio::test]
    async fn test_git_rebasing() {
        let dir = create_git_repo().await;
        // Simulate an in-progress rebase by creating the rebase-merge directory
        std::fs::create_dir_all(dir.path().join(".git/rebase-merge")).unwrap();

        let config = Config {
            color: false,
            ..Default::default()
        };
        let status = query_git_status(dir.path(), &config).await.unwrap();
        assert!(status.rebasing, "expected rebasing to be true");
    }

    #[tokio::test]
    async fn test_git_rebase_apply() {
        let dir = create_git_repo().await;
        // Simulate an in-progress rebase-apply (non-interactive rebase / am)
        std::fs::create_dir_all(dir.path().join(".git/rebase-apply")).unwrap();

        let config = Config {
            color: false,
            ..Default::default()
        };
        let status = query_git_status(dir.path(), &config).await.unwrap();
        assert!(
            status.rebasing,
            "expected rebasing to be true for rebase-apply"
        );
    }

    #[tokio::test]
    async fn test_git_format_with_branch() {
        use crate::template::format_status;
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

    /// Parse the summary line from `git diff --stat` output.
    fn parse_diff_stat_summary(output: &str) -> (u32, u32, u32) {
        let Some(summary) = output.lines().rev().find(|l| l.contains("changed")) else {
            return (0, 0, 0);
        };
        let mut files = 0u32;
        let mut insertions = 0u32;
        let mut deletions = 0u32;
        for part in summary.split(',') {
            let part = part.trim();
            if part.contains("changed") {
                files = part
                    .split_whitespace()
                    .next()
                    .and_then(|n| n.parse().ok())
                    .unwrap_or(0);
            } else if part.contains("insertion") {
                insertions = part
                    .split_whitespace()
                    .next()
                    .and_then(|n| n.parse().ok())
                    .unwrap_or(0);
            } else if part.contains("deletion") {
                deletions = part
                    .split_whitespace()
                    .next()
                    .and_then(|n| n.parse().ok())
                    .unwrap_or(0);
            }
        }
        (files, insertions, deletions)
    }

    async fn git_output(repo: &std::path::Path, args: &[&str]) -> String {
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
        String::from_utf8_lossy(&output.stdout).to_string()
    }

    /// Complex scenario: multiple files added, deleted, and modified.
    /// Compares unstaged, staged, and total stats against git CLI.
    #[tokio::test]
    async fn test_diff_stats_match_git_cli() {
        let dir = create_git_repo().await;

        // Create initial files and commit
        std::fs::create_dir_all(dir.path().join("src")).unwrap();

        let main_initial: String = (1..=25).map(|i| format!("fn app_{i}() {{}}\n")).collect();
        std::fs::write(dir.path().join("src/app.rs"), &main_initial).unwrap();

        let config_initial: String =
            (1..=10).map(|i| format!("config_key_{i} = value\n")).collect();
        std::fs::write(dir.path().join("src/config.rs"), &config_initial).unwrap();

        let guide_initial: String = (1..=15).map(|i| format!("## Guide step {i}\n")).collect();
        std::fs::write(dir.path().join("guide.md"), &guide_initial).unwrap();

        let makefile_initial: String =
            (1..=8).map(|i| format!("target_{i}:\n\techo {i}\n")).collect();
        std::fs::write(dir.path().join("Makefile"), &makefile_initial).unwrap();

        let old_content: String = (1..=6).map(|i| format!("old line {i}\n")).collect();
        std::fs::write(dir.path().join("old.txt"), &old_content).unwrap();

        git_cmd(dir.path(), &["add", "."]).await;
        git_cmd(dir.path(), &["commit", "-m", "add initial files"]).await;

        // --- Make modifications ---

        // 1. New file: src/helper.rs (10 lines)
        let helper: String = (1..=10).map(|i| format!("fn helper_{i}() {{}}\n")).collect();
        std::fs::write(dir.path().join("src/helper.rs"), &helper).unwrap();

        // 2. Delete old.txt
        std::fs::remove_file(dir.path().join("old.txt")).unwrap();

        // 3. Modify src/app.rs: change lines 5-8, add 3 at end
        let mut app_lines: Vec<String> = (1..=25).map(|i| format!("fn app_{i}() {{}}")).collect();
        app_lines[4] = "fn app_5_changed() { /* new */ }".to_string();
        app_lines[5] = "fn app_6_changed() { /* new */ }".to_string();
        app_lines[6] = "fn app_7_changed() { /* new */ }".to_string();
        app_lines[7] = "fn app_8_changed() { /* new */ }".to_string();
        app_lines.push("fn app_26() {}".to_string());
        app_lines.push("fn app_27() {}".to_string());
        app_lines.push("fn app_28() {}".to_string());
        std::fs::write(dir.path().join("src/app.rs"), app_lines.join("\n") + "\n").unwrap();

        // 4. Modify guide.md: remove lines 10-15, add 4 new lines
        let mut guide_lines: Vec<String> = (1..=9).map(|i| format!("## Guide step {i}")).collect();
        guide_lines.push("## New guide A".to_string());
        guide_lines.push("## New guide B".to_string());
        guide_lines.push("## New guide C".to_string());
        guide_lines.push("## New guide D".to_string());
        std::fs::write(dir.path().join("guide.md"), guide_lines.join("\n") + "\n").unwrap();

        // 5. Modify Makefile: change 2 lines
        let mut make_lines: Vec<String> =
            (1..=8).map(|i| format!("target_{i}:\n\techo {i}")).collect();
        make_lines[2] = "target_3_new:\n\techo changed_3".to_string();
        make_lines[5] = "target_6_new:\n\techo changed_6".to_string();
        std::fs::write(dir.path().join("Makefile"), make_lines.join("\n") + "\n").unwrap();

        // --- Compare total stats (HEAD → workdir, nothing staged) ---
        let git_total = git_output(dir.path(), &["diff", "--stat", "HEAD"]).await;
        let (cli_total_f, cli_total_a, cli_total_r) = parse_diff_stat_summary(&git_total);

        let config = Config {
            color: false,
            ..Default::default()
        };
        let status = query_git_status(dir.path(), &config).await.unwrap();

        assert_eq!(
            (
                status.total_files_changed,
                status.total_lines_added,
                status.total_lines_removed
            ),
            (cli_total_f, cli_total_a, cli_total_r),
            "total stats ({}f, +{}, -{}) != git diff --stat HEAD ({}f, +{}, -{})\ngit output:\n{}",
            status.total_files_changed,
            status.total_lines_added,
            status.total_lines_removed,
            cli_total_f,
            cli_total_a,
            cli_total_r,
            git_total,
        );

        // Unstaged should match too (nothing is staged)
        let git_unstaged = git_output(dir.path(), &["diff", "--stat"]).await;
        let (cli_us_f, cli_us_a, cli_us_r) = parse_diff_stat_summary(&git_unstaged);

        assert_eq!(
            (
                status.files_changed,
                status.lines_added,
                status.lines_removed
            ),
            (cli_us_f, cli_us_a, cli_us_r),
            "unstaged stats ({}f, +{}, -{}) != git diff --stat ({}f, +{}, -{})\ngit output:\n{}",
            status.files_changed,
            status.lines_added,
            status.lines_removed,
            cli_us_f,
            cli_us_a,
            cli_us_r,
            git_unstaged,
        );
    }

    /// Test with a mix of staged and unstaged changes, verifying all three
    /// stat categories separately against git CLI.
    #[tokio::test]
    async fn test_diff_stats_match_git_cli_staged_and_unstaged() {
        let dir = create_git_repo().await;

        // Initial committed state: two files
        let alpha: String = (1..=20).map(|i| format!("alpha line {i}\n")).collect();
        std::fs::write(dir.path().join("alpha.txt"), &alpha).unwrap();

        let beta: String = (1..=15).map(|i| format!("beta line {i}\n")).collect();
        std::fs::write(dir.path().join("beta.txt"), &beta).unwrap();

        git_cmd(dir.path(), &["add", "."]).await;
        git_cmd(dir.path(), &["commit", "-m", "add alpha and beta"]).await;

        // Stage changes to alpha.txt: change lines 3-5, add 2 lines
        let mut alpha_staged: Vec<String> =
            (1..=20).map(|i| format!("alpha line {i}")).collect();
        alpha_staged[2] = "alpha STAGED 3".to_string();
        alpha_staged[3] = "alpha STAGED 4".to_string();
        alpha_staged[4] = "alpha STAGED 5".to_string();
        alpha_staged.push("alpha STAGED new 1".to_string());
        alpha_staged.push("alpha STAGED new 2".to_string());
        std::fs::write(
            dir.path().join("alpha.txt"),
            alpha_staged.join("\n") + "\n",
        )
        .unwrap();
        git_cmd(dir.path(), &["add", "alpha.txt"]).await;

        // Now make further unstaged changes to alpha.txt on top of staged
        let mut alpha_unstaged = alpha_staged.clone();
        alpha_unstaged[9] = "alpha UNSTAGED 10".to_string();
        alpha_unstaged[10] = "alpha UNSTAGED 11".to_string();
        std::fs::write(
            dir.path().join("alpha.txt"),
            alpha_unstaged.join("\n") + "\n",
        )
        .unwrap();

        // Unstaged changes to beta.txt (not staged at all): remove last 5 lines
        let beta_modified: String = (1..=10).map(|i| format!("beta line {i}\n")).collect();
        std::fs::write(dir.path().join("beta.txt"), &beta_modified).unwrap();

        // Add a new staged file
        std::fs::write(dir.path().join("gamma.txt"), "gamma 1\ngamma 2\ngamma 3\n").unwrap();
        git_cmd(dir.path(), &["add", "gamma.txt"]).await;

        // Compare all three categories
        let config = Config {
            color: false,
            ..Default::default()
        };
        let status = query_git_status(dir.path(), &config).await.unwrap();

        // Unstaged: index → workdir
        let git_unstaged = git_output(dir.path(), &["diff", "--stat"]).await;
        let (cli_us_f, cli_us_a, cli_us_r) = parse_diff_stat_summary(&git_unstaged);
        assert_eq!(
            (
                status.files_changed,
                status.lines_added,
                status.lines_removed
            ),
            (cli_us_f, cli_us_a, cli_us_r),
            "unstaged ({}f, +{}, -{}) != git diff --stat ({}f, +{}, -{})\n{}",
            status.files_changed,
            status.lines_added,
            status.lines_removed,
            cli_us_f,
            cli_us_a,
            cli_us_r,
            git_unstaged,
        );

        // Staged: HEAD → index
        let git_staged = git_output(dir.path(), &["diff", "--cached", "--stat"]).await;
        let (cli_st_f, cli_st_a, cli_st_r) = parse_diff_stat_summary(&git_staged);
        assert_eq!(
            (
                status.staged_files_changed,
                status.staged_lines_added,
                status.staged_lines_removed
            ),
            (cli_st_f, cli_st_a, cli_st_r),
            "staged ({}f, +{}, -{}) != git diff --cached --stat ({}f, +{}, -{})\n{}",
            status.staged_files_changed,
            status.staged_lines_added,
            status.staged_lines_removed,
            cli_st_f,
            cli_st_a,
            cli_st_r,
            git_staged,
        );

        // Total: HEAD → workdir+index
        let git_total = git_output(dir.path(), &["diff", "--stat", "HEAD"]).await;
        let (cli_tot_f, cli_tot_a, cli_tot_r) = parse_diff_stat_summary(&git_total);
        assert_eq!(
            (
                status.total_files_changed,
                status.total_lines_added,
                status.total_lines_removed
            ),
            (cli_tot_f, cli_tot_a, cli_tot_r),
            "total ({}f, +{}, -{}) != git diff --stat HEAD ({}f, +{}, -{})\n{}",
            status.total_files_changed,
            status.total_lines_added,
            status.total_lines_removed,
            cli_tot_f,
            cli_tot_a,
            cli_tot_r,
            git_total,
        );
    }
}
