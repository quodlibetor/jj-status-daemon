use anyhow::{Context, Result};
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use std::path::{Path, PathBuf};
use tokio::sync::mpsc;

#[derive(Debug, Clone)]
pub struct WatchEvent {
    pub repo_path: PathBuf,
    pub working_copy_changed: bool,
}

pub struct RepoWatcher {
    _watcher: RecommendedWatcher,
}

pub fn watch_repo(
    repo_path: &Path,
    tx: mpsc::UnboundedSender<WatchEvent>,
) -> Result<RepoWatcher> {
    let op_heads_dir = repo_path.join(".jj/repo/op_heads/heads");
    let repo_path_owned = repo_path.to_path_buf();
    let jj_dir = repo_path.join(".jj");

    let mut watcher = notify::recommended_watcher(move |res: std::result::Result<Event, notify::Error>| {
        let Ok(event) = res else { return };

        // Skip non-modification events
        if !event.kind.is_modify() && !event.kind.is_create() && !event.kind.is_remove() {
            return;
        }

        // Determine if this is a working copy change or a .jj internal change
        let working_copy_changed = event.paths.iter().any(|p| {
            // If the changed path is not under .jj/, it's a working copy change
            !p.starts_with(&jj_dir)
        });

        // Filter out target/ directory changes
        let dominated_by_target = event.paths.iter().all(|p| {
            p.strip_prefix(&repo_path_owned)
                .map(|rel| rel.starts_with("target"))
                .unwrap_or(false)
        });
        if dominated_by_target {
            return;
        }

        let _ = tx.send(WatchEvent {
            repo_path: repo_path_owned.clone(),
            working_copy_changed,
        });
    })?;

    // Watch op_heads for jj operations
    if op_heads_dir.exists() {
        watcher
            .watch(&op_heads_dir, RecursiveMode::NonRecursive)
            .context("failed to watch op_heads")?;
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
    use tokio::time::{timeout, Duration};

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
        let _watcher = watch_repo(dir.path(), tx).unwrap();

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
        assert_eq!(event.repo_path, dir.path());
    }

    #[tokio::test]
    async fn test_watcher_detects_file_change() {
        let dir = create_jj_repo().await;
        let (tx, mut rx) = mpsc::unbounded_channel();
        let _watcher = watch_repo(dir.path(), tx).unwrap();

        // Write a file to working copy
        tokio::fs::write(dir.path().join("hello.txt"), "hello").await.unwrap();

        // Drain events looking for a working_copy_changed=true event
        let mut found = false;
        for _ in 0..20 {
            match timeout(Duration::from_secs(5), rx.recv()).await {
                Ok(Some(event)) => {
                    if event.working_copy_changed {
                        found = true;
                        break;
                    }
                }
                _ => break,
            }
        }
        assert!(found, "expected working_copy_changed event");
    }
}
