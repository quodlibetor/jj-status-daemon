use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::sync::mpsc;

use crate::config::Config;
use crate::jj::{
    DiffCounts, FileChangeKind, FileDiffStats, abs_to_repo_relative, aggregate_overlay_stats,
    aggregate_overlay_stats_by_dir,
};
use crate::template::RepoStatus;

const GIT2_TIMEOUT: Duration = Duration::from_secs(30);

fn diff_stats(diff: &git2::Diff<'_>) -> Result<DiffCounts> {
    let stats = diff.stats()?;
    let mut counts = DiffCounts {
        file_mad_count: stats.files_changed() as u32,
        lines_added: stats.insertions() as u32,
        lines_removed: stats.deletions() as u32,
        ..Default::default()
    };
    // Count per-delta status categories
    for i in 0..diff.deltas().len() {
        if let Some(delta) = diff.get_delta(i) {
            match delta.status() {
                git2::Delta::Added => counts.files_added += 1,
                git2::Delta::Deleted => counts.files_deleted += 1,
                git2::Delta::Untracked => counts.files_untracked += 1,
                _ => counts.files_modified += 1,
            }
        }
    }
    Ok(counts)
}

fn delta_to_kind(status: git2::Delta) -> FileChangeKind {
    match status {
        git2::Delta::Added => FileChangeKind::Added,
        git2::Delta::Deleted => FileChangeKind::Deleted,
        git2::Delta::Untracked => FileChangeKind::Untracked,
        _ => FileChangeKind::Modified,
    }
}

/// Extract per-file line-level diff stats from a git2 Diff.
fn per_file_stats_from_diff(diff: &git2::Diff<'_>) -> Result<HashMap<String, FileDiffStats>> {
    let mut result: HashMap<String, FileDiffStats> = HashMap::new();

    diff.print(git2::DiffFormat::Patch, |delta, _hunk, line| {
        let path = delta
            .new_file()
            .path()
            .or_else(|| delta.old_file().path())
            .map(|p| p.to_string_lossy().to_string());
        if let Some(path) = path {
            let stats = result.entry(path).or_insert_with(|| FileDiffStats {
                kind: delta_to_kind(delta.status()),
                ..Default::default()
            });
            match line.origin() {
                '+' => stats.lines_added += 1,
                '-' => stats.lines_removed += 1,
                _ => {}
            }
        }
        true
    })?;

    // Also ensure entries exist for files with no line content (binary, pure renames)
    for i in 0..diff.deltas().len() {
        if let Some(delta) = diff.get_delta(i) {
            let path = delta
                .new_file()
                .path()
                .or_else(|| delta.old_file().path())
                .map(|p| p.to_string_lossy().to_string());
            if let Some(path) = path {
                result.entry(path).or_insert_with(|| FileDiffStats {
                    kind: delta_to_kind(delta.status()),
                    ..Default::default()
                });
            }
        }
    }

    Ok(result)
}

/// Aggregate per-file stats into DiffCounts.
fn aggregate_file_stats(per_file: &HashMap<String, FileDiffStats>) -> DiffCounts {
    let empty = HashMap::new();
    aggregate_overlay_stats(per_file, &empty)
}

/// Retained git state for incremental working copy diffs.
pub struct GitRepoState {
    /// OID of the HEAD tree (for total diffs: HEAD → workdir).
    head_tree_oid: Option<git2::Oid>,
    repo_root: PathBuf,
    base_status: RepoStatus,
    /// Per-file stats from index → workdir diff (unstaged).
    base_unstaged: HashMap<String, FileDiffStats>,
    /// Per-file stats from HEAD tree → workdir+index diff (total).
    base_total: HashMap<String, FileDiffStats>,
    /// Overlay for unstaged diffs (working copy changes only).
    unstaged_overlay: HashMap<String, Option<FileDiffStats>>,
    /// Overlay for total diffs (working copy changes only).
    total_overlay: HashMap<String, Option<FileDiffStats>>,
}

impl GitRepoState {
    /// Build a RepoStatus with current aggregate diff stats from base + overlay.
    fn current_status(&self) -> RepoStatus {
        let us = aggregate_overlay_stats(&self.base_unstaged, &self.unstaged_overlay);
        let tot = aggregate_overlay_stats(&self.base_total, &self.total_overlay);
        RepoStatus {
            file_mad_count_working_tree: us.file_mad_count,
            lines_added_working_tree: us.lines_added,
            lines_removed_working_tree: us.lines_removed,
            files_modified_working_tree: us.files_modified,
            files_added_working_tree: us.files_added,
            files_deleted_working_tree: us.files_deleted,
            file_mad_count: tot.file_mad_count,
            lines_added_total: tot.lines_added,
            lines_removed_total: tot.lines_removed,
            files_modified_total: tot.files_modified,
            files_added_total: tot.files_added,
            files_deleted_total: tot.files_deleted,
            untracked: tot.files_untracked,
            // Staged stats are unchanged by working copy edits
            ..self.base_status.clone()
        }
    }

    /// Incrementally update overlays for the given changed files.
    fn update_files(&mut self, changed_paths: &[PathBuf]) {
        let rel_paths: Vec<String> = changed_paths
            .iter()
            .filter_map(|p| abs_to_repo_relative(&self.repo_root, p))
            .collect();

        if rel_paths.is_empty() {
            return;
        }

        // Re-open the repo to get fresh stat/index state.
        // This is cheap (just reads .git/HEAD, config, index) and avoids
        // stale stat-cache issues that prevent detecting working copy changes.
        let repo = match git2::Repository::open(&self.repo_root) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "failed to re-open git repo for incremental update");
                return;
            }
        };

        let head_tree = self.head_tree_oid.and_then(|oid| repo.find_tree(oid).ok());

        // Batch unstaged diff: index → workdir with pathspec filter
        {
            let mut opts = git2::DiffOptions::new();
            opts.include_untracked(true);
            for path in &rel_paths {
                opts.pathspec(path);
            }
            match repo.diff_index_to_workdir(None, Some(&mut opts)) {
                Ok(diff) => {
                    let stats_map = per_file_stats_from_diff(&diff).unwrap_or_default();
                    for path in &rel_paths {
                        self.unstaged_overlay
                            .insert(path.clone(), stats_map.get(path).cloned());
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "incremental unstaged diff failed");
                }
            }
        }

        // Batch total diff: HEAD tree → workdir+index with pathspec filter
        {
            let mut opts = git2::DiffOptions::new();
            opts.include_untracked(true);
            for path in &rel_paths {
                opts.pathspec(path);
            }
            match repo.diff_tree_to_workdir_with_index(head_tree.as_ref(), Some(&mut opts)) {
                Ok(diff) => {
                    let stats_map = per_file_stats_from_diff(&diff).unwrap_or_default();
                    for path in &rel_paths {
                        self.total_overlay
                            .insert(path.clone(), stats_map.get(path).cloned());
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "incremental total diff failed");
                }
            }
        }
    }
}

#[tracing::instrument(fields(repo = %repo_path.display()))]
fn query_git_status_blocking(repo_path: &Path) -> Result<RepoStatus> {
    let (status, _state) = query_git_status_blocking_with_state(repo_path)?;
    Ok(status)
}

/// Full git status query that also returns retained state for incremental updates.
#[tracing::instrument(fields(repo = %repo_path.display()))]
fn query_git_status_blocking_with_state(repo_path: &Path) -> Result<(RepoStatus, GitRepoState)> {
    let mut repo = {
        let _span = tracing::debug_span!("git_open").entered();
        git2::Repository::open(repo_path).context("failed to open git repo")?
    };

    let mut status = RepoStatus {
        is_git: true,
        ..Default::default()
    };

    // Check for unborn HEAD
    if repo.head().is_err() {
        let repo_root = repo_path
            .canonicalize()
            .unwrap_or_else(|_| repo_path.to_path_buf());
        let state = GitRepoState {
            head_tree_oid: None,
            repo_root,
            base_status: status.clone(),
            base_unstaged: HashMap::new(),
            base_total: HashMap::new(),
            unstaged_overlay: HashMap::new(),
            total_overlay: HashMap::new(),
        };
        return Ok((status, state));
    }

    let (head_tree_oid, base_unstaged, base_total) = {
        // head() is Ok — we checked above
        let head = repo.head().unwrap();

        // Branch name (empty when HEAD is detached)
        status.branch = if head.is_branch() {
            head.shorthand().unwrap_or("").to_string()
        } else {
            String::new()
        };

        // Commit ID (short), description, and tree for diff stats
        let head_tree_oid = if let Ok(commit) = head.peel_to_commit() {
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
            head_tree.map(|t| t.id())
        } else {
            None
        };

        // Re-lookup the head tree by OID (so `head` and `commit` can be dropped)
        let head_tree = head_tree_oid.and_then(|oid| repo.find_tree(oid).ok());

        // Conflict detection
        status.conflict = repo.index().map(|idx| idx.has_conflicts()).unwrap_or(false);

        // Unstaged: index → workdir (with per-file stats)
        let base_unstaged = {
            let _span = tracing::debug_span!("diff_unstaged").entered();
            let mut diff_opts = git2::DiffOptions::new();
            diff_opts.include_untracked(true);
            if let Ok(diff) = repo.diff_index_to_workdir(None, Some(&mut diff_opts)) {
                let per_file = per_file_stats_from_diff(&diff).unwrap_or_default();
                let c = aggregate_file_stats(&per_file);
                status.file_mad_count_working_tree = c.file_mad_count;
                status.lines_added_working_tree = c.lines_added;
                status.lines_removed_working_tree = c.lines_removed;
                status.files_modified_working_tree = c.files_modified;
                status.files_added_working_tree = c.files_added;
                status.files_deleted_working_tree = c.files_deleted;
                per_file
            } else {
                HashMap::new()
            }
        };

        // Staged: tree → index
        {
            let _span = tracing::debug_span!("diff_staged").entered();
            if let Ok(diff) = repo.diff_tree_to_index(head_tree.as_ref(), None, None)
                && let Ok(c) = diff_stats(&diff)
            {
                status.file_mad_count_staged = c.file_mad_count;
                status.lines_added_staged = c.lines_added;
                status.lines_removed_staged = c.lines_removed;
                status.files_modified_staged = c.files_modified;
                status.files_added_staged = c.files_added;
                status.files_deleted_staged = c.files_deleted;
            }
        }

        // Total: tree → workdir (with per-file stats)
        let base_total = {
            let _span = tracing::debug_span!("diff_total").entered();
            let mut total_opts = git2::DiffOptions::new();
            total_opts.include_untracked(true);
            if let Ok(diff) =
                repo.diff_tree_to_workdir_with_index(head_tree.as_ref(), Some(&mut total_opts))
            {
                let per_file = per_file_stats_from_diff(&diff).unwrap_or_default();
                let c = aggregate_file_stats(&per_file);
                status.file_mad_count = c.file_mad_count;
                status.lines_added_total = c.lines_added;
                status.lines_removed_total = c.lines_removed;
                status.files_modified_total = c.files_modified;
                status.files_added_total = c.files_added;
                status.files_deleted_total = c.files_deleted;
                status.untracked = c.files_untracked;
                per_file
            } else {
                HashMap::new()
            }
        };

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

        (head_tree_oid, base_unstaged, base_total)
    };

    // Ahead/behind: compare local branch to its upstream tracking branch
    if !status.branch.is_empty()
        && let Ok(local) = repo.find_branch(&status.branch, git2::BranchType::Local)
        && let Ok(upstream) = local.upstream()
        && let Some(local_oid) = local.get().target()
        && let Some(upstream_oid) = upstream.get().target()
        && let Ok((ahead, behind)) = repo.graph_ahead_behind(local_oid, upstream_oid)
    {
        status.ahead = ahead as u32;
        status.behind = behind as u32;
    }

    // Stash count
    {
        let mut count: u32 = 0;
        let _ = repo.stash_foreach(|_index, _message, _oid| {
            count += 1;
            true
        });
        status.stashes = count;
    }

    let repo_root = repo_path
        .canonicalize()
        .unwrap_or_else(|_| repo_path.to_path_buf());

    let state = GitRepoState {
        head_tree_oid,
        repo_root,
        base_status: status.clone(),
        base_unstaged,
        base_total,
        unstaged_overlay: HashMap::new(),
        total_overlay: HashMap::new(),
    };

    Ok((status, state))
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

/// Requests that can be sent to the git worker thread.
pub enum GitWorkerRequest {
    /// Full refresh: re-open repo, compute all status fields.
    FullRefresh {
        repo_path: PathBuf,
        reply: tokio::sync::oneshot::Sender<Result<RepoStatus>>,
    },
    /// Incremental update: diff specific working copy files using retained state.
    IncrementalUpdate {
        repo_path: PathBuf,
        changed_paths: Vec<PathBuf>,
        reply: tokio::sync::oneshot::Sender<Result<RepoStatus>>,
    },
    /// Query current overlay stats for all repos.
    QueryOverlayStats {
        reply: tokio::sync::oneshot::Sender<Vec<(String, crate::protocol::IncrementalDiffStats)>>,
    },
    /// Query per-directory overlay stats for all repos.
    QueryOverlayStatsVerbose {
        reply: tokio::sync::oneshot::Sender<crate::protocol::VerboseDirStats>,
    },
}

/// Spawn a dedicated blocking thread that owns git2 Repository state.
///
/// Returns a sender for submitting requests. The worker exits when the sender is dropped.
pub fn spawn_git_worker() -> mpsc::UnboundedSender<GitWorkerRequest> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    tokio::task::spawn_blocking(move || {
        let mut states: HashMap<PathBuf, GitRepoState> = HashMap::new();

        // Blocking recv loop — runs on a dedicated OS thread
        while let Some(req) = rx.blocking_recv() {
            match req {
                GitWorkerRequest::FullRefresh { repo_path, reply } => {
                    let result = query_git_status_blocking_with_state(&repo_path);
                    match result {
                        Ok((status, git_state)) => {
                            states.insert(repo_path, git_state);
                            let _ = reply.send(Ok(status));
                        }
                        Err(e) => {
                            let _ = reply.send(Err(e));
                        }
                    }
                }
                GitWorkerRequest::IncrementalUpdate {
                    repo_path,
                    changed_paths,
                    reply,
                } => {
                    let Some(state) = states.get_mut(&repo_path) else {
                        let _ =
                            reply.send(Err(anyhow::anyhow!("no incremental state for git repo")));
                        continue;
                    };
                    let _span = tracing::debug_span!("git_incremental_update",
                        repo = %repo_path.display(),
                        files = changed_paths.len()
                    )
                    .entered();

                    state.update_files(&changed_paths);
                    let status = state.current_status();
                    let _ = reply.send(Ok(status));
                }
                GitWorkerRequest::QueryOverlayStats { reply } => {
                    let stats: Vec<_> = states
                        .iter()
                        .map(|(path, state)| {
                            let counts =
                                aggregate_overlay_stats(&state.base_total, &state.total_overlay);
                            (
                                path.to_string_lossy().to_string(),
                                crate::protocol::IncrementalDiffStats {
                                    base_files: state.base_total.len() as u32,
                                    overlay_entries: state.total_overlay.len() as u32,
                                    files_changed: counts.file_mad_count,
                                    lines_added: counts.lines_added,
                                    lines_removed: counts.lines_removed,
                                },
                            )
                        })
                        .collect();
                    let _ = reply.send(stats);
                }
                GitWorkerRequest::QueryOverlayStatsVerbose { reply } => {
                    let stats: Vec<_> = states
                        .iter()
                        .map(|(path, state)| {
                            let dir_stats = aggregate_overlay_stats_by_dir(
                                &state.base_total,
                                &state.total_overlay,
                            );
                            (path.to_string_lossy().to_string(), dir_stats)
                        })
                        .collect();
                    let _ = reply.send(stats);
                }
            }
        }
    });
    tx
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use tokio::process::Command;

    use crate::test_util::create_git_repo_async as create_git_repo;

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
            status.file_mad_count_working_tree >= 1,
            "expected unstaged file_mad_count_working_tree >= 1, got {}",
            status.file_mad_count_working_tree
        );
        assert!(
            status.lines_added_working_tree > 0,
            "expected unstaged lines_added_working_tree > 0, got {}",
            status.lines_added_working_tree
        );
        // Staged: nothing staged
        assert_eq!(status.file_mad_count_staged, 0);
        // Total: same as unstaged since nothing is staged
        assert_eq!(status.file_mad_count, status.file_mad_count_working_tree);
        assert_eq!(status.lines_added_total, status.lines_added_working_tree);
    }

    #[tokio::test]
    async fn test_git_untracked_files() {
        let dir = create_git_repo().await;
        // Create an untracked file (not added to index)
        std::fs::write(dir.path().join("new_file.txt"), "hello\n").unwrap();
        let config = Config {
            color: false,
            ..Default::default()
        };
        let status = query_git_status(dir.path(), &config).await.unwrap();
        assert_eq!(status.untracked, 1, "expected 1 untracked file");
        // Untracked files should not count as files_changed or affect line stats
        assert_eq!(status.file_mad_count, 0);
        assert_eq!(status.lines_added_total, 0);
        assert_eq!(status.lines_removed_total, 0);
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
            status.file_mad_count_working_tree, 0,
            "expected no unstaged changes, got file_mad_count_working_tree={}",
            status.file_mad_count_working_tree
        );
        // Staged: should show the change
        assert!(
            status.file_mad_count_staged >= 1,
            "expected file_mad_count_staged >= 1, got {}",
            status.file_mad_count_staged
        );
        assert!(
            status.lines_added_staged > 0,
            "expected lines_added_staged > 0, got {}",
            status.lines_added_staged
        );
        // Total: same as staged
        assert_eq!(status.file_mad_count, status.file_mad_count_staged);
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
            status.file_mad_count_working_tree >= 1,
            "expected unstaged file_mad_count_working_tree >= 1, got {}",
            status.file_mad_count_working_tree
        );
        assert!(
            status.file_mad_count_staged >= 1,
            "expected file_mad_count_staged >= 1, got {}",
            status.file_mad_count_staged
        );
        assert!(
            status.file_mad_count >= 1,
            "expected file_mad_count >= 1, got {}",
            status.file_mad_count
        );
        // Total lines should be >= staged + unstaged (though file counts may not add)
        assert!(
            status.lines_added_total >= status.lines_added_staged,
            "lines_added_total ({}) should be >= lines_added_staged ({})",
            status.lines_added_total,
            status.lines_added_staged
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
    use crate::test_util::parse_diff_stat_summary;

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

        let config_initial: String = (1..=10)
            .map(|i| format!("config_key_{i} = value\n"))
            .collect();
        std::fs::write(dir.path().join("src/config.rs"), &config_initial).unwrap();

        let guide_initial: String = (1..=15).map(|i| format!("## Guide step {i}\n")).collect();
        std::fs::write(dir.path().join("guide.md"), &guide_initial).unwrap();

        let makefile_initial: String = (1..=8)
            .map(|i| format!("target_{i}:\n\techo {i}\n"))
            .collect();
        std::fs::write(dir.path().join("Makefile"), &makefile_initial).unwrap();

        let old_content: String = (1..=6).map(|i| format!("old line {i}\n")).collect();
        std::fs::write(dir.path().join("old.txt"), &old_content).unwrap();

        git_cmd(dir.path(), &["add", "."]).await;
        git_cmd(dir.path(), &["commit", "-m", "add initial files"]).await;

        // --- Make modifications ---

        // 1. New file: src/helper.rs (10 lines)
        let helper: String = (1..=10)
            .map(|i| format!("fn helper_{i}() {{}}\n"))
            .collect();
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
        let mut make_lines: Vec<String> = (1..=8)
            .map(|i| format!("target_{i}:\n\techo {i}"))
            .collect();
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
                status.file_mad_count,
                status.lines_added_total,
                status.lines_removed_total
            ),
            (cli_total_f, cli_total_a, cli_total_r),
            "total stats ({}f, +{}, -{}) != git diff --stat HEAD ({}f, +{}, -{})\ngit output:\n{}",
            status.file_mad_count,
            status.lines_added_total,
            status.lines_removed_total,
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
                status.file_mad_count_working_tree,
                status.lines_added_working_tree,
                status.lines_removed_working_tree
            ),
            (cli_us_f, cli_us_a, cli_us_r),
            "unstaged stats ({}f, +{}, -{}) != git diff --stat ({}f, +{}, -{})\ngit output:\n{}",
            status.file_mad_count_working_tree,
            status.lines_added_working_tree,
            status.lines_removed_working_tree,
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
        let mut alpha_staged: Vec<String> = (1..=20).map(|i| format!("alpha line {i}")).collect();
        alpha_staged[2] = "alpha STAGED 3".to_string();
        alpha_staged[3] = "alpha STAGED 4".to_string();
        alpha_staged[4] = "alpha STAGED 5".to_string();
        alpha_staged.push("alpha STAGED new 1".to_string());
        alpha_staged.push("alpha STAGED new 2".to_string());
        std::fs::write(dir.path().join("alpha.txt"), alpha_staged.join("\n") + "\n").unwrap();
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
                status.file_mad_count_working_tree,
                status.lines_added_working_tree,
                status.lines_removed_working_tree
            ),
            (cli_us_f, cli_us_a, cli_us_r),
            "unstaged ({}f, +{}, -{}) != git diff --stat ({}f, +{}, -{})\n{}",
            status.file_mad_count_working_tree,
            status.lines_added_working_tree,
            status.lines_removed_working_tree,
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
                status.file_mad_count_staged,
                status.lines_added_staged,
                status.lines_removed_staged
            ),
            (cli_st_f, cli_st_a, cli_st_r),
            "staged ({}f, +{}, -{}) != git diff --cached --stat ({}f, +{}, -{})\n{}",
            status.file_mad_count_staged,
            status.lines_added_staged,
            status.lines_removed_staged,
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
                status.file_mad_count,
                status.lines_added_total,
                status.lines_removed_total
            ),
            (cli_tot_f, cli_tot_a, cli_tot_r),
            "total ({}f, +{}, -{}) != git diff --stat HEAD ({}f, +{}, -{})\n{}",
            status.file_mad_count,
            status.lines_added_total,
            status.lines_removed_total,
            cli_tot_f,
            cli_tot_a,
            cli_tot_r,
            git_total,
        );
    }

    // --- Per-file stats extraction tests ---

    #[tokio::test]
    async fn test_per_file_stats_extraction() {
        let dir = create_git_repo().await;
        // Create and commit two files
        std::fs::write(dir.path().join("a.txt"), "line1\nline2\nline3\n").unwrap();
        std::fs::write(dir.path().join("b.txt"), "alpha\nbeta\n").unwrap();
        git_cmd(dir.path(), &["add", "."]).await;
        git_cmd(dir.path(), &["commit", "-m", "two files"]).await;

        // Modify both files
        std::fs::write(dir.path().join("a.txt"), "line1\nmodified\nline3\nnew\n").unwrap();
        std::fs::write(dir.path().join("b.txt"), "alpha\nbeta\ngamma\n").unwrap();

        let repo = git2::Repository::open(dir.path()).unwrap();
        let mut opts = git2::DiffOptions::new();
        let diff = repo.diff_index_to_workdir(None, Some(&mut opts)).unwrap();
        let per_file = per_file_stats_from_diff(&diff).unwrap();

        assert!(per_file.contains_key("a.txt"), "should have a.txt");
        assert!(per_file.contains_key("b.txt"), "should have b.txt");

        let a = &per_file["a.txt"];
        assert!(a.lines_added > 0, "a.txt should have additions");
        assert!(a.lines_removed > 0, "a.txt should have removals");

        let b = &per_file["b.txt"];
        assert_eq!(b.lines_added, 1, "b.txt should have 1 addition");
        assert_eq!(b.lines_removed, 0, "b.txt should have 0 removals");

        // Aggregate should match diff_stats
        let agg = aggregate_file_stats(&per_file);
        let ds = diff_stats(&diff).unwrap();
        assert_eq!(
            (agg.file_mad_count, agg.lines_added, agg.lines_removed),
            (ds.file_mad_count, ds.lines_added, ds.lines_removed)
        );
    }

    // --- GitRepoState incremental update tests ---

    #[tokio::test]
    async fn test_git_incremental_modify_file() {
        let dir = create_git_repo().await;
        std::fs::write(dir.path().join("src.txt"), "line1\nline2\nline3\n").unwrap();
        git_cmd(dir.path(), &["add", "."]).await;
        git_cmd(dir.path(), &["commit", "-m", "add src"]).await;

        // Full refresh with no working copy changes
        let (base_status, mut state) = query_git_status_blocking_with_state(dir.path()).unwrap();
        assert_eq!(base_status.file_mad_count, 0);

        // Modify the file
        std::fs::write(dir.path().join("src.txt"), "line1\nMODIFIED\nline3\nnew\n").unwrap();

        // Incremental update
        state.update_files(&[dir.path().join("src.txt")]);
        let updated = state.current_status();

        assert_eq!(updated.file_mad_count, 1);
        assert!(updated.lines_added_total > 0);
        assert!(updated.lines_removed_total > 0);
        // Unstaged should also reflect the change
        assert_eq!(updated.file_mad_count_working_tree, 1);
    }

    #[tokio::test]
    async fn test_git_incremental_new_tracked_file() {
        let dir = create_git_repo().await;

        // Stage a new file (so it appears in the index)
        std::fs::write(dir.path().join("new.txt"), "hello\nworld\n").unwrap();
        git_cmd(dir.path(), &["add", "new.txt"]).await;

        // Base with staged new file
        let (base_status, mut state) = query_git_status_blocking_with_state(dir.path()).unwrap();
        assert!(
            base_status.file_mad_count_staged >= 1,
            "expected staged changes"
        );

        // Now modify the staged file further (unstaged change)
        std::fs::write(dir.path().join("new.txt"), "hello\nworld\nmore\n").unwrap();

        state.update_files(&[dir.path().join("new.txt")]);
        let updated = state.current_status();

        // Should detect the unstaged modification
        assert!(
            updated.file_mad_count_working_tree >= 1,
            "expected unstaged change, got file_mad_count_working_tree={}",
            updated.file_mad_count_working_tree
        );
    }

    #[tokio::test]
    async fn test_git_incremental_delete_file() {
        let dir = create_git_repo().await;
        std::fs::write(dir.path().join("doomed.txt"), "goodbye\nworld\n").unwrap();
        git_cmd(dir.path(), &["add", "."]).await;
        git_cmd(dir.path(), &["commit", "-m", "add doomed"]).await;

        let (_base, mut state) = query_git_status_blocking_with_state(dir.path()).unwrap();

        // Delete the file
        std::fs::remove_file(dir.path().join("doomed.txt")).unwrap();

        state.update_files(&[dir.path().join("doomed.txt")]);
        let updated = state.current_status();

        assert_eq!(updated.file_mad_count, 1);
        assert!(updated.lines_removed_total > 0);
        assert_eq!(updated.lines_added_total, 0);
    }

    #[tokio::test]
    async fn test_git_incremental_revert_file() {
        let dir = create_git_repo().await;
        std::fs::write(dir.path().join("src.txt"), "original\n").unwrap();
        git_cmd(dir.path(), &["add", "."]).await;
        git_cmd(dir.path(), &["commit", "-m", "add src"]).await;

        // Modify the file, take a base snapshot
        std::fs::write(dir.path().join("src.txt"), "modified\n").unwrap();
        let (_base, mut state) = query_git_status_blocking_with_state(dir.path()).unwrap();
        assert_eq!(state.base_status.file_mad_count, 1);

        // Revert the file back to committed content
        std::fs::write(dir.path().join("src.txt"), "original\n").unwrap();

        state.update_files(&[dir.path().join("src.txt")]);
        let updated = state.current_status();

        // Should show no changes since file matches HEAD
        assert_eq!(
            updated.file_mad_count, 0,
            "expected 0 file_mad_count after revert, got {}",
            updated.file_mad_count
        );
    }

    #[tokio::test]
    async fn test_git_incremental_multiple_files() {
        let dir = create_git_repo().await;
        std::fs::write(dir.path().join("a.txt"), "aaa\n").unwrap();
        std::fs::write(dir.path().join("b.txt"), "bbb\n").unwrap();
        std::fs::write(dir.path().join("c.txt"), "ccc\n").unwrap();
        git_cmd(dir.path(), &["add", "."]).await;
        git_cmd(dir.path(), &["commit", "-m", "three files"]).await;

        let (_base, mut state) = query_git_status_blocking_with_state(dir.path()).unwrap();

        // Modify a and c, leave b unchanged
        std::fs::write(dir.path().join("a.txt"), "AAA\nNEW\n").unwrap();
        std::fs::write(dir.path().join("c.txt"), "CCC\n").unwrap();

        state.update_files(&[
            dir.path().join("a.txt"),
            dir.path().join("b.txt"),
            dir.path().join("c.txt"),
        ]);
        let updated = state.current_status();

        assert_eq!(updated.file_mad_count, 2, "expected 2 changed files");
    }

    #[tokio::test]
    async fn test_git_incremental_matches_full_refresh() {
        let dir = create_git_repo().await;
        std::fs::write(dir.path().join("x.txt"), "line1\nline2\nline3\n").unwrap();
        std::fs::write(dir.path().join("y.txt"), "alpha\nbeta\n").unwrap();
        git_cmd(dir.path(), &["add", "."]).await;
        git_cmd(dir.path(), &["commit", "-m", "two files"]).await;

        // Take base with clean state
        let (_base, mut state) = query_git_status_blocking_with_state(dir.path()).unwrap();

        // Make changes
        std::fs::write(dir.path().join("x.txt"), "line1\nMOD\nline3\nNEW\n").unwrap();
        std::fs::remove_file(dir.path().join("y.txt")).unwrap();
        std::fs::write(dir.path().join("z.txt"), "new file\n").unwrap();

        // Incremental update
        state.update_files(&[
            dir.path().join("x.txt"),
            dir.path().join("y.txt"),
            dir.path().join("z.txt"),
        ]);
        let incremental = state.current_status();

        // Full refresh for comparison
        let config = Config {
            color: false,
            ..Default::default()
        };
        let full = query_git_status(dir.path(), &config).await.unwrap();

        assert_eq!(
            (
                incremental.file_mad_count,
                incremental.lines_added_total,
                incremental.lines_removed_total
            ),
            (
                full.file_mad_count,
                full.lines_added_total,
                full.lines_removed_total
            ),
            "incremental total ({}f, +{}, -{}) != full ({}f, +{}, -{})",
            incremental.file_mad_count,
            incremental.lines_added_total,
            incremental.lines_removed_total,
            full.file_mad_count,
            full.lines_added_total,
            full.lines_removed_total,
        );
    }

    #[tokio::test]
    async fn test_git_incremental_preserves_staged_stats() {
        let dir = create_git_repo().await;
        std::fs::write(dir.path().join("staged.txt"), "line1\n").unwrap();
        std::fs::write(dir.path().join("wc.txt"), "aaa\n").unwrap();
        git_cmd(dir.path(), &["add", "."]).await;
        git_cmd(dir.path(), &["commit", "-m", "initial"]).await;

        // Stage a change
        std::fs::write(dir.path().join("staged.txt"), "line1\nstaged line\n").unwrap();
        git_cmd(dir.path(), &["add", "staged.txt"]).await;

        // Take base
        let (base, mut state) = query_git_status_blocking_with_state(dir.path()).unwrap();
        assert!(base.file_mad_count_staged >= 1, "expected staged changes");

        // Modify a working copy file
        std::fs::write(dir.path().join("wc.txt"), "modified\n").unwrap();

        state.update_files(&[dir.path().join("wc.txt")]);
        let updated = state.current_status();

        // Staged stats should be unchanged
        assert_eq!(
            updated.file_mad_count_staged, base.file_mad_count_staged,
            "file_mad_count_staged should be preserved"
        );
        assert_eq!(
            updated.lines_added_staged, base.lines_added_staged,
            "lines_added_staged should be preserved"
        );
        // Unstaged should show the working copy change
        assert!(
            updated.file_mad_count_working_tree >= 1,
            "expected unstaged changes"
        );
    }

    // --- Git worker integration tests ---

    #[tokio::test]
    async fn test_git_worker_full_and_incremental() {
        let dir = create_git_repo().await;
        std::fs::write(dir.path().join("f.txt"), "one\ntwo\nthree\n").unwrap();
        git_cmd(dir.path(), &["add", "."]).await;
        git_cmd(dir.path(), &["commit", "-m", "file"]).await;

        let worker = spawn_git_worker();

        // Full refresh
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        worker
            .send(GitWorkerRequest::FullRefresh {
                repo_path: dir.path().to_path_buf(),
                reply: reply_tx,
            })
            .unwrap();
        let status = reply_rx.await.unwrap().unwrap();
        assert_eq!(status.file_mad_count, 0);

        // Modify and do incremental update
        std::fs::write(dir.path().join("f.txt"), "one\nMODIFIED\nthree\nfour\n").unwrap();

        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        worker
            .send(GitWorkerRequest::IncrementalUpdate {
                repo_path: dir.path().to_path_buf(),
                changed_paths: vec![dir.path().join("f.txt")],
                reply: reply_tx,
            })
            .unwrap();
        let status = reply_rx.await.unwrap().unwrap();
        assert_eq!(status.file_mad_count, 1);
        assert!(status.lines_added_total > 0);
    }

    #[tokio::test]
    async fn test_git_worker_incremental_without_state_fails() {
        let dir = create_git_repo().await;
        let worker = spawn_git_worker();

        // Try incremental without full refresh first — should fail
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        worker
            .send(GitWorkerRequest::IncrementalUpdate {
                repo_path: dir.path().to_path_buf(),
                changed_paths: vec![dir.path().join("f.txt")],
                reply: reply_tx,
            })
            .unwrap();
        let result = reply_rx.await.unwrap();
        assert!(result.is_err(), "expected error without prior full refresh");
    }
}
