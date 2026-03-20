use anyhow::{Context, Result};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::mpsc;

use crate::protocol::VcsKind;

pub enum WatchEvent {
    Change {
        repo_path: PathBuf,
        vcs_kind: VcsKind,
        working_copy_changed: bool,
        /// Absolute paths of changed files (non-ignored working copy files only).
        changed_paths: Vec<PathBuf>,
    },
    Flush(tokio::sync::oneshot::Sender<()>),
}

pub struct RepoWatcher {
    _watcher: RecommendedWatcher,
    /// Count of filesystem events skipped because all paths matched ignore rules.
    pub ignored_events: Arc<AtomicU64>,
}

/// Build a gitignore matcher from the repo's ignore files.
///
/// For jj repos: loads `.gitignore` and `.jjignore` (jj respects both).
/// For git repos: loads `.gitignore`.
/// Also loads nested ignore files aren't handled here — just the root-level ones,
/// which covers the vast majority of noisy paths (target/, node_modules/, .build/, etc.).
fn build_ignore(repo_path: &Path, vcs_kind: VcsKind) -> Gitignore {
    let canonical = repo_path
        .canonicalize()
        .unwrap_or_else(|_| repo_path.to_path_buf());
    let mut builder = GitignoreBuilder::new(&canonical);

    // Always add .gitignore (both jj and git respect it)
    let gitignore = repo_path.join(".gitignore");
    if gitignore.exists() {
        builder.add(gitignore);
    }

    if vcs_kind == VcsKind::Jj {
        let jjignore = repo_path.join(".jjignore");
        if jjignore.exists() {
            builder.add(jjignore);
        }
    }

    // Also load the global gitignore if available
    if let Some(global) = global_gitignore_path()
        && global.exists()
    {
        builder.add(global);
    }

    builder.build().unwrap_or_else(|_| {
        // If parsing fails, return an empty matcher (nothing ignored)
        GitignoreBuilder::new(repo_path).build().unwrap()
    })
}

/// Find the global gitignore path from git config or the default location.
fn global_gitignore_path() -> Option<PathBuf> {
    // Try `core.excludesFile` via git config
    let output = std::process::Command::new("git")
        .args(["config", "--global", "core.excludesFile"])
        .output()
        .ok()?;
    if output.status.success() {
        let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !path.is_empty() {
            // Expand ~ if present
            if let Some(rest) = path.strip_prefix("~/")
                && let Some(home) = dirs::home_dir()
            {
                return Some(home.join(rest));
            }
            return Some(PathBuf::from(path));
        }
    }

    // Default: ~/.config/git/ignore
    dirs::home_dir().map(|h| h.join(".config/git/ignore"))
}

pub fn watch_repo(
    repo_path: &Path,
    vcs_kind: VcsKind,
    tx: mpsc::UnboundedSender<WatchEvent>,
) -> Result<RepoWatcher> {
    let repo_path_owned = repo_path.to_path_buf();
    let canonical_root = repo_path
        .canonicalize()
        .unwrap_or_else(|_| repo_path.to_path_buf());
    let vcs_dir = match vcs_kind {
        VcsKind::Jj => canonical_root.join(".jj"),
        VcsKind::Git => canonical_root.join(".git"),
    };
    let gitignore = build_ignore(repo_path, vcs_kind);
    let ignored_events = Arc::new(AtomicU64::new(0));
    let ignored_events_cb = ignored_events.clone();

    let mut watcher =
        notify::recommended_watcher(move |res: std::result::Result<Event, notify::Error>| {
            let Ok(event) = res else { return };

            // Skip non-modification events
            if !event.kind.is_modify() && !event.kind.is_create() && !event.kind.is_remove() {
                return;
            }

            // Determine if this is a working copy change or a VCS internal change
            let working_copy_changed = event.paths.iter().any(|p| !p.starts_with(&vcs_dir));

            // For working copy events, filter out ignored paths
            if working_copy_changed {
                let all_ignored = event.paths.iter().all(|p| {
                    if p.starts_with(&vcs_dir) {
                        return false; // VCS internal paths are never "ignored"
                    }
                    // Strip the canonical root to get a relative path for matching.
                    // Use matched_path_or_any_parents so that a file inside an
                    // ignored directory (e.g. build/output.o matching "build/")
                    // is correctly detected as ignored.
                    let rel = p.strip_prefix(&canonical_root).unwrap_or(p);
                    let is_dir = p.is_dir();
                    gitignore
                        .matched_path_or_any_parents(rel, is_dir)
                        .is_ignore()
                });
                if all_ignored {
                    ignored_events_cb.fetch_add(1, Ordering::Relaxed);
                    return;
                }
            }

            // Collect non-VCS, non-ignored working copy paths for incremental diffs
            let changed_paths: Vec<PathBuf> = if working_copy_changed {
                event
                    .paths
                    .iter()
                    .filter(|p| {
                        if p.starts_with(&vcs_dir) {
                            return false;
                        }
                        let rel = p.strip_prefix(&canonical_root).unwrap_or(p);
                        let is_dir = p.is_dir();
                        !gitignore
                            .matched_path_or_any_parents(rel, is_dir)
                            .is_ignore()
                    })
                    .cloned()
                    .collect()
            } else {
                Vec::new()
            };
            let _ = tx.send(WatchEvent::Change {
                repo_path: repo_path_owned.clone(),
                vcs_kind,
                working_copy_changed,
                changed_paths,
            });
        })?;

    match vcs_kind {
        VcsKind::Jj => {
            // Watch op_heads for jj operations
            let op_heads_dir = repo_path.join(".jj/repo/op_heads/heads");
            if op_heads_dir.exists() {
                watcher
                    .watch(&op_heads_dir, RecursiveMode::NonRecursive)
                    .context("failed to watch op_heads")?;
            }
        }
        VcsKind::Git => {
            // Watch .git/ for ref changes, HEAD, index
            let git_dir = repo_path.join(".git");
            if git_dir.is_dir() {
                watcher
                    .watch(&git_dir, RecursiveMode::NonRecursive)
                    .context("failed to watch .git")?;
                let refs_dir = git_dir.join("refs");
                if refs_dir.is_dir() {
                    watcher
                        .watch(&refs_dir, RecursiveMode::Recursive)
                        .context("failed to watch .git/refs")?;
                }
            }
        }
    }

    // Watch working directory for file changes
    watcher
        .watch(repo_path, RecursiveMode::Recursive)
        .context("failed to watch repo")?;

    Ok(RepoWatcher {
        _watcher: watcher,
        ignored_events,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use tokio::process::Command;
    use tokio::time::{Duration, timeout};

    async fn create_jj_repo() -> TempDir {
        let dir = TempDir::new().unwrap();
        let output = Command::new("jj")
            .args(["git", "init"])
            .current_dir(dir.path())
            .output()
            .await
            .expect("failed to run `jj git init` — is `jj` installed and in PATH?");
        assert!(
            output.status.success(),
            "jj git init failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        dir
    }

    #[tokio::test]
    async fn test_watcher_detects_jj_op() {
        let dir = create_jj_repo().await;
        let (tx, mut rx) = mpsc::unbounded_channel();
        let _watcher = watch_repo(dir.path(), VcsKind::Jj, tx).unwrap();

        // Make a jj operation
        Command::new("jj")
            .args(["describe", "-m", "test"])
            .current_dir(dir.path())
            .output()
            .await
            .unwrap();

        // Should receive at least one event
        let event = timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("timeout waiting for watch event")
            .expect("channel closed");
        match event {
            WatchEvent::Change { repo_path, .. } => assert_eq!(repo_path, dir.path()),
            WatchEvent::Flush(_) => panic!("unexpected Flush event"),
        }
    }

    #[tokio::test]
    async fn test_watcher_detects_file_change() {
        let dir = create_jj_repo().await;
        let (tx, mut rx) = mpsc::unbounded_channel();
        let _watcher = watch_repo(dir.path(), VcsKind::Jj, tx).unwrap();

        // Write a file to working copy
        tokio::fs::write(dir.path().join("hello.txt"), "hello")
            .await
            .unwrap();

        // Drain events looking for a working_copy_changed=true event
        let mut found = false;
        for _ in 0..20 {
            match timeout(Duration::from_secs(5), rx.recv()).await {
                Ok(Some(WatchEvent::Change {
                    working_copy_changed: true,
                    ..
                })) => {
                    found = true;
                    break;
                }
                Ok(Some(_)) => continue,
                _ => break,
            }
        }
        assert!(found, "expected working_copy_changed event");
    }

    #[tokio::test]
    async fn test_watcher_ignores_gitignored_files() {
        let dir = create_jj_repo().await;

        // Create .gitignore and build/ directory BEFORE starting the watcher
        // so these writes don't generate events
        std::fs::write(dir.path().join(".gitignore"), "build/\n").unwrap();
        std::fs::create_dir(dir.path().join("build")).unwrap();

        // Small delay to let any jj-internal events from init settle
        tokio::time::sleep(Duration::from_millis(500)).await;

        let (tx, mut rx) = mpsc::unbounded_channel();
        let _watcher = watch_repo(dir.path(), VcsKind::Jj, tx).unwrap();

        // Drain any startup events
        tokio::time::sleep(Duration::from_millis(500)).await;
        while rx.try_recv().is_ok() {}

        // Write to an ignored path — should NOT produce a working_copy_changed event
        tokio::fs::write(dir.path().join("build/output.o"), "binary")
            .await
            .unwrap();

        // Give the watcher time to fire
        tokio::time::sleep(Duration::from_millis(500)).await;

        // Drain: we should not see any working_copy_changed=true events
        let mut saw_wc_change = false;
        while let Ok(event) = rx.try_recv() {
            if let WatchEvent::Change {
                working_copy_changed: true,
                ..
            } = event
            {
                saw_wc_change = true;
            }
        }
        assert!(
            !saw_wc_change,
            "should not see working_copy_changed for gitignored file"
        );
    }

    #[tokio::test]
    async fn test_watcher_passes_tracked_files_with_ignore_active() {
        let dir = create_jj_repo().await;

        // Create .gitignore and build/ directory BEFORE starting the watcher
        std::fs::write(dir.path().join(".gitignore"), "build/\n").unwrap();
        std::fs::create_dir(dir.path().join("build")).unwrap();

        // Let jj-internal events from init settle
        tokio::time::sleep(Duration::from_millis(500)).await;

        let (tx, mut rx) = mpsc::unbounded_channel();
        let _watcher = watch_repo(dir.path(), VcsKind::Jj, tx).unwrap();

        // Drain any startup events
        tokio::time::sleep(Duration::from_millis(500)).await;
        while rx.try_recv().is_ok() {}

        // 1) Write to an ignored path — should NOT produce a working_copy_changed event
        tokio::fs::write(dir.path().join("build/output.o"), "binary")
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;

        let mut saw_ignored = false;
        while let Ok(event) = rx.try_recv() {
            if let WatchEvent::Change {
                working_copy_changed: true,
                ..
            } = event
            {
                saw_ignored = true;
            }
        }
        assert!(
            !saw_ignored,
            "should not see working_copy_changed for gitignored file"
        );

        // 2) Write to a tracked path — SHOULD produce a working_copy_changed event
        tokio::fs::write(dir.path().join("src.txt"), "code")
            .await
            .unwrap();

        let mut found = false;
        for _ in 0..20 {
            match timeout(Duration::from_secs(5), rx.recv()).await {
                Ok(Some(WatchEvent::Change {
                    working_copy_changed: true,
                    ..
                })) => {
                    found = true;
                    break;
                }
                Ok(Some(_)) => continue,
                _ => break,
            }
        }
        assert!(
            found,
            "expected working_copy_changed for tracked file while ignore is active"
        );
    }
}
