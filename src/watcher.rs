use anyhow::{Context, Result};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
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

/// Result of processing an event's paths through the ignore filter.
pub struct EventVerdict {
    /// All working-copy paths in the event are ignored — skip the event entirely.
    pub all_ignored: bool,
    /// Non-ignored working-copy paths for incremental diffs.
    pub changed_paths: Vec<PathBuf>,
}

/// Lazily discovers and incorporates nested `.gitignore`/`.jjignore` files as
/// filesystem events arrive. Thread-safe: `Arc<Mutex<>>` is internal, all public
/// methods handle locking.
#[derive(Clone)]
pub struct IgnoreFilter {
    inner: Arc<Mutex<IgnoreFilterInner>>,
}

struct IgnoreFilterInner {
    matcher: Gitignore,
    /// Relative directories (from repo root) we have already probed for ignore files.
    checked_dirs: HashSet<PathBuf>,
    /// Absolute paths of all ignore files currently loaded into the matcher.
    loaded_files: HashSet<PathBuf>,
    canonical_root: PathBuf,
    vcs_kind: VcsKind,
    global_ignore: Option<PathBuf>,
}

impl IgnoreFilter {
    /// Create a new filter, loading root-level ignore files and the global gitignore.
    pub fn new(repo_path: &Path, vcs_kind: VcsKind) -> Self {
        let canonical_root = repo_path
            .canonicalize()
            .unwrap_or_else(|_| repo_path.to_path_buf());
        let global_ignore = global_gitignore_path().filter(|g| g.exists());

        let mut loaded_files = HashSet::new();

        // Always add root .gitignore (both jj and git respect it)
        let gitignore = canonical_root.join(".gitignore");
        if gitignore.exists() {
            loaded_files.insert(gitignore);
        }

        if vcs_kind == VcsKind::Jj {
            let jjignore = canonical_root.join(".jjignore");
            if jjignore.exists() {
                loaded_files.insert(jjignore);
            }
        }

        let mut checked_dirs = HashSet::new();
        checked_dirs.insert(PathBuf::new()); // root dir is checked

        let matcher = Self::build_matcher(&canonical_root, &loaded_files, global_ignore.as_deref());

        IgnoreFilter {
            inner: Arc::new(Mutex::new(IgnoreFilterInner {
                matcher,
                checked_dirs,
                loaded_files,
                canonical_root,
                vcs_kind,
                global_ignore,
            })),
        }
    }

    /// Process an event's paths: lazily discover ignore files, then filter.
    ///
    /// Ignore files are incorporated *before* filtering so the matcher is always
    /// up-to-date. Only working-copy paths (outside `vcs_dir`) are considered.
    pub fn process_event(&self, vcs_dir: &Path, paths: &[PathBuf]) -> EventVerdict {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());

        // Phase 1: discover and incorporate any new ignore files before filtering.
        inner.discover_ignore_files(paths, vcs_dir);

        // Phase 2: filter paths.
        let canonical_root = inner.canonical_root.clone();
        let wc_paths: Vec<&PathBuf> = paths.iter().filter(|p| !p.starts_with(vcs_dir)).collect();

        if wc_paths.is_empty() {
            return EventVerdict {
                all_ignored: false,
                changed_paths: Vec::new(),
            };
        }

        let all_ignored = wc_paths.iter().all(|p| {
            let rel = p.strip_prefix(&canonical_root).unwrap_or(p);
            let is_dir = p.is_dir();
            inner
                .matcher
                .matched_path_or_any_parents(rel, is_dir)
                .is_ignore()
        });

        let changed_paths = if all_ignored {
            Vec::new()
        } else {
            wc_paths
                .into_iter()
                .filter(|p| {
                    let rel = p.strip_prefix(&canonical_root).unwrap_or(p);
                    let is_dir = p.is_dir();
                    !inner
                        .matcher
                        .matched_path_or_any_parents(rel, is_dir)
                        .is_ignore()
                })
                .cloned()
                .collect()
        };

        EventVerdict {
            all_ignored,
            changed_paths,
        }
    }

    /// Build a `Gitignore` matcher from all loaded files plus the global ignore.
    fn build_matcher(
        canonical_root: &Path,
        loaded_files: &HashSet<PathBuf>,
        global_ignore: Option<&Path>,
    ) -> Gitignore {
        let mut builder = GitignoreBuilder::new(canonical_root);

        if let Some(global) = global_ignore {
            builder.add(global);
        }

        // Sort by path depth (shallowest first) so deeper files override shallower
        // via addition order — the ignore crate gives later additions higher priority.
        let mut sorted: Vec<&PathBuf> = loaded_files.iter().collect();
        sorted.sort_by_key(|p| p.components().count());
        for file in sorted {
            builder.add(file);
        }

        builder
            .build()
            .unwrap_or_else(|_| GitignoreBuilder::new(canonical_root).build().unwrap())
    }
}

impl IgnoreFilterInner {
    /// Check event paths for new ignore files and rebuild the matcher if needed.
    fn discover_ignore_files(&mut self, paths: &[PathBuf], vcs_dir: &Path) {
        let mut needs_rebuild = false;

        for path in paths {
            // Skip VCS-internal paths
            if path.starts_with(vcs_dir) {
                continue;
            }

            // Check if this path IS an ignore file (create/modify/delete)
            if self.is_ignore_filename(path) {
                if path.exists() {
                    // Created or modified — add/re-add
                    self.loaded_files.insert(path.clone());
                    needs_rebuild = true;
                } else if self.loaded_files.remove(path) {
                    // Deleted — remove
                    needs_rebuild = true;
                }
            }

            // Walk ancestor directories from this path up to repo root,
            // probing unchecked directories for ignore files.
            if let Ok(rel) = path.strip_prefix(&self.canonical_root) {
                // Iterate over ancestor directories of the relative path
                let mut dir = rel;
                loop {
                    dir = match dir.parent() {
                        Some(d) => d,
                        None => break,
                    };
                    if dir.as_os_str().is_empty() {
                        break; // reached repo root, already checked at init
                    }
                    if !self.checked_dirs.insert(dir.to_path_buf()) {
                        // Already checked this dir (and therefore all its ancestors)
                        break;
                    }

                    let abs_dir = self.canonical_root.join(dir);
                    let gitignore = abs_dir.join(".gitignore");
                    if gitignore.exists() && self.loaded_files.insert(gitignore) {
                        needs_rebuild = true;
                    }
                    if self.vcs_kind == VcsKind::Jj {
                        let jjignore = abs_dir.join(".jjignore");
                        if jjignore.exists() && self.loaded_files.insert(jjignore) {
                            needs_rebuild = true;
                        }
                    }
                }
            }
        }

        if needs_rebuild {
            self.matcher = IgnoreFilter::build_matcher(
                &self.canonical_root,
                &self.loaded_files,
                self.global_ignore.as_deref(),
            );
        }
    }

    fn is_ignore_filename(&self, path: &Path) -> bool {
        let Some(name) = path.file_name() else {
            return false;
        };
        name == ".gitignore" || (self.vcs_kind == VcsKind::Jj && name == ".jjignore")
    }
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
    let filter = IgnoreFilter::new(repo_path, vcs_kind);
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

            // Lazily discover ignore files and filter paths in one step.
            // Ignore files are incorporated before filtering so the matcher
            // is always up-to-date when we check paths.
            let verdict = filter.process_event(&vcs_dir, &event.paths);
            if working_copy_changed && verdict.all_ignored {
                ignored_events_cb.fetch_add(1, Ordering::Relaxed);
                return;
            }

            let _ = tx.send(WatchEvent::Change {
                repo_path: repo_path_owned.clone(),
                vcs_kind,
                working_copy_changed,
                changed_paths: verdict.changed_paths,
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
    use tokio::process::Command;
    use tokio::time::{Duration, timeout};

    use crate::test_util::create_jj_repo_async as create_jj_repo;

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

    #[tokio::test]
    async fn test_watcher_discovers_nested_gitignore() {
        let dir = create_jj_repo().await;

        // Create sub/.gitignore that ignores *.log BEFORE starting the watcher
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/.gitignore"), "*.log\n").unwrap();

        tokio::time::sleep(Duration::from_millis(500)).await;

        let (tx, mut rx) = mpsc::unbounded_channel();
        let _watcher = watch_repo(dir.path(), VcsKind::Jj, tx).unwrap();

        // Drain startup events
        tokio::time::sleep(Duration::from_millis(500)).await;
        while rx.try_recv().is_ok() {}

        // Write to an ignored path in sub/ — should NOT produce working_copy_changed
        tokio::fs::write(dir.path().join("sub/debug.log"), "log data")
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;

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
            "should not see working_copy_changed for file matched by nested .gitignore"
        );

        // Write to a non-ignored path in sub/ — SHOULD produce working_copy_changed
        tokio::fs::write(dir.path().join("sub/code.rs"), "fn main() {}")
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
            "expected working_copy_changed for non-ignored file in sub/"
        );
    }

    #[tokio::test]
    async fn test_watcher_lazy_discovers_new_gitignore() {
        let dir = create_jj_repo().await;

        tokio::time::sleep(Duration::from_millis(500)).await;

        let (tx, mut rx) = mpsc::unbounded_channel();
        let _watcher = watch_repo(dir.path(), VcsKind::Jj, tx).unwrap();

        // Drain startup events
        tokio::time::sleep(Duration::from_millis(500)).await;
        while rx.try_recv().is_ok() {}

        // Create sub/ and sub/.gitignore — the watcher discovers it lazily
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/.gitignore"), "*.o\n").unwrap();

        // Let the watcher process the .gitignore creation events
        tokio::time::sleep(Duration::from_millis(500)).await;
        while rx.try_recv().is_ok() {}

        // Now write to a path that the new ignore file should ignore
        tokio::fs::write(dir.path().join("sub/output.o"), "binary")
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;

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
            "should not see working_copy_changed for file matched by lazily-discovered .gitignore"
        );
    }

    #[tokio::test]
    async fn test_nested_gitignore_no_sibling_effect() {
        let dir = create_jj_repo().await;

        // Create sub_a/.gitignore that ignores *.tmp, but sub_b/ has no ignore file
        std::fs::create_dir(dir.path().join("sub_a")).unwrap();
        std::fs::write(dir.path().join("sub_a/.gitignore"), "*.tmp\n").unwrap();
        std::fs::create_dir(dir.path().join("sub_b")).unwrap();

        tokio::time::sleep(Duration::from_millis(500)).await;

        let (tx, mut rx) = mpsc::unbounded_channel();
        let _watcher = watch_repo(dir.path(), VcsKind::Jj, tx).unwrap();

        // Drain startup events
        tokio::time::sleep(Duration::from_millis(500)).await;
        while rx.try_recv().is_ok() {}

        // Write *.tmp in sub_b/ — should NOT be ignored (sibling directory)
        tokio::fs::write(dir.path().join("sub_b/file.tmp"), "temp data")
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
        assert!(found, "sub_a/.gitignore should not affect files in sub_b/");
    }
}
