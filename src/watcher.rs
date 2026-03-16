use anyhow::{Context, Result};
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use std::path::{Path, PathBuf};
use tokio::sync::mpsc;

use crate::protocol::VcsKind;

pub enum WatchEvent {
    Change {
        repo_path: PathBuf,
        vcs_kind: VcsKind,
        working_copy_changed: bool,
    },
    Flush(tokio::sync::oneshot::Sender<()>),
}

pub struct RepoWatcher {
    _watcher: RecommendedWatcher,
}

pub fn watch_repo(
    repo_path: &Path,
    vcs_kind: VcsKind,
    tx: mpsc::UnboundedSender<WatchEvent>,
) -> Result<RepoWatcher> {
    let repo_path_owned = repo_path.to_path_buf();
    let vcs_dir = match vcs_kind {
        VcsKind::Jj => repo_path.join(".jj"),
        VcsKind::Git => repo_path.join(".git"),
    };

    let mut watcher =
        notify::recommended_watcher(move |res: std::result::Result<Event, notify::Error>| {
            let Ok(event) = res else { return };

            // Skip non-modification events
            if !event.kind.is_modify() && !event.kind.is_create() && !event.kind.is_remove() {
                return;
            }

            // Determine if this is a working copy change or a VCS internal change
            let working_copy_changed = event.paths.iter().any(|p| !p.starts_with(&vcs_dir));

            // Filter out target/ directory changes
            let dominated_by_target = event.paths.iter().all(|p| {
                p.strip_prefix(&repo_path_owned)
                    .map(|rel| rel.starts_with("target"))
                    .unwrap_or(false)
            });
            if dominated_by_target {
                return;
            }

            let _ = tx.send(WatchEvent::Change {
                repo_path: repo_path_owned.clone(),
                vcs_kind,
                working_copy_changed,
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

    Ok(RepoWatcher { _watcher: watcher })
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
            .unwrap();
        assert!(output.status.success());
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
}
