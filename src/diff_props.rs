//! Property-testing engine for the jj incremental diff engine.
//!
//! Generates random sequences of filesystem mutations and jj commands,
//! replays them against a real jj repo, and drives the `JjWorkerRequest`
//! state machine the same way the daemon would — synthesizing the watcher
//! event stream from an exact before/after disk mirror. After every action
//! the worker's diff stats are compared against the oracle: `jj diff --stat`.
//!
//! Two event-delivery modes:
//! - **Perfect**: every changed file is delivered, like a lossless watcher.
//!   This is the CI suite — it must always pass.
//! - **Lossy**: event batches are dropped per the generated sequence,
//!   simulating FSEvents overflow / directory-level coalescing. The system
//!   is then required to *converge* after the next op event (which rebuilds
//!   base stats whenever the working-copy commit tree changed).

use proptest::prelude::*;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use crate::config::Config;
use crate::jj::{JjWorkerRequest, spawn_jj_worker};
use crate::template::RepoStatus;
use crate::test_util::{create_jj_repo, parse_diff_stat_summary};

/// Fixed pool of file paths that actions index into. Small enough that
/// actions frequently collide on the same file (modify-after-add, etc).
/// `x.log` exists to interact with the `*.log` gitignore pattern.
const FILE_POOL: &[&str] = &[
    "a.txt",
    "b.txt",
    "dir/c.txt",
    "dir/sub/d.txt",
    "e.rs",
    "x.log",
];

/// Root `.gitignore` contents that `WriteGitignore` cycles through. Each
/// interacts with part of FILE_POOL: untracked matches disappear from
/// diffs, already-tracked matches must remain.
const GITIGNORE_VARIANTS: &[&str] = &["*.log\n", "dir/\n", "e.rs\n", "*.log\ndir/\n"];

#[derive(Debug, Clone)]
pub enum Action {
    /// Overwrite (or create) a file with `lines` seeded lines.
    Write {
        file: usize,
        lines: u8,
        seed: u8,
    },
    /// Append `lines` seeded lines to a file (no-op if the file is absent).
    Append {
        file: usize,
        lines: u8,
        seed: u8,
    },
    /// Rewrite one line in place (no-op if the file is absent/empty).
    Touch {
        file: usize,
        line: u8,
        seed: u8,
    },
    /// Delete a file (no-op if absent).
    Delete {
        file: usize,
    },
    /// jj commands. `EditPrior(n)` edits the n-th mutable ancestor-ish change.
    JjNew,
    JjCommit,
    JjDescribe {
        seed: u8,
    },
    JjAbandon,
    JjSquash,
    JjUndo,
    JjEditPrior {
        nth: u8,
    },
    /// `jj restore` — rewrite the whole working copy from the parent.
    JjRestoreAll,
    /// `jj restore <file>` — restore one pool file from the parent.
    JjRestoreFile {
        file: usize,
    },
    /// `std::fs::rename` between two pool paths (delete+add to jj).
    RenameFile {
        from: usize,
        to: usize,
    },
    /// Rename the `dir/` tree to `dir2/` or back — a directory-level move
    /// whose per-file events some platforms coalesce.
    MoveDir,
    /// `jj split -m msg -- <file>` — carve one file into a new parent commit.
    JjSplit {
        file: usize,
    },
    /// `jj rebase -r @ -d <nth visible commit>` — reparent @, possibly
    /// creating conflicts.
    JjRebase {
        nth: u8,
    },
    /// `jj new @ <nth other head>` — create a merge commit (megamerge leg;
    /// conflicts arise organically when heads touched the same file).
    JjNewMerge {
        nth: u8,
    },
    /// `jj config set --repo ui.conflict-marker-style <style>`. Skipped when
    /// @ is conflicted: jj deliberately does not rematerialize existing
    /// on-disk conflicts on config change, so toggling under a live conflict
    /// leaves old-style markers that neither jj nor the daemon can trust.
    SetMarkerStyle {
        style: u8,
    },
    /// Commit seeded content to the `main` branch of the side "remote" git
    /// repo. Invisible locally until a fetch.
    RemoteCommit {
        file: usize,
        lines: u8,
        seed: u8,
    },
    /// `jj git fetch` — imports remote refs; op event with no WC change.
    JjGitFetch,
    /// `jj new main@origin` — start a change on fetched remote work: a
    /// checkout that rewrites the working copy (the classic drift trigger).
    JjNewOnRemote,
    /// Write a root `.gitignore` (lossy suite only: making an existing
    /// untracked file ignored — or un-ignoring one by deletion — is only
    /// observable to the daemon at the next operation, since no watcher
    /// event fires for files whose ignore status flips).
    WriteGitignore {
        variant: u8,
    },
    /// Delete the root `.gitignore` (lossy suite only, see WriteGitignore).
    DeleteGitignore,
    /// Lossy mode only: drop all not-yet-delivered file events, simulating
    /// FSEvents queue overflow. In Perfect mode this is skipped.
    DropPendingEvents,
    /// chmod +x / -x on a pool file (no-op if absent). jj counts an
    /// exec-bit-only change as one changed file with 0 line changes
    /// (`file | 0`).
    SetExecBit {
        file: usize,
        on: bool,
    },
    /// Create the second workspace (`jj workspace add --name second`) in its
    /// own TempDir. No-op if it already exists.
    WorkspaceAdd,
    /// Write a file in the SECOND workspace and snapshot it there — an
    /// operation foreign to the watched workspace (op_heads change, no
    /// watched-disk change). If the second workspace is stale, recover it
    /// first (`jj workspace update-stale`) like a user would.
    WorkspaceOp {
        file: usize,
        lines: u8,
        seed: u8,
    },
    /// From the WATCHED workspace, rewrite the second workspace's @ with a
    /// tree change (`jj restore --from @ --into second@ -- <file>`). Leaves
    /// the OTHER workspace stale; ours stays healthy (same-workspace
    /// commands always update their own checkout). Describe-only rewrites
    /// would NOT stale it: jj auto-heals when the wc-commit tree is
    /// unchanged.
    RewriteOtherWsAncestor {
        file: usize,
    },
    /// From the SECOND workspace, rewrite a mutable commit on the watched
    /// @'s line (possibly watched @ itself) with a tree change — makes the
    /// WATCHED workspace stale. Runs with --ignore-working-copy so it works
    /// even when the second workspace is itself stale.
    RewriteWatchedWsAncestor {
        file: usize,
        nth: u8,
    },
    /// `jj workspace update-stale` in the watched workspace: snapshots +
    /// checks out, rewriting disk files with no user edit. Safe no-op when
    /// healthy ("Attempted recovery, but the working copy is not stale").
    WorkspaceUpdateStale,
    /// Forget the SECOND workspace (never the watched one). No-op if absent.
    WorkspaceForget,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    Perfect,
    Lossy,
}

/// Strategy for the CI-strict action set: every action's effect must be
/// reflected in the daemon's stats immediately (per-action oracle parity).
fn safe_action() -> impl Strategy<Value = Action> {
    prop_oneof![
        6 => (0..FILE_POOL.len(), 1..12u8, any::<u8>())
            .prop_map(|(file, lines, seed)| Action::Write { file, lines, seed }),
        4 => (0..FILE_POOL.len(), 1..6u8, any::<u8>())
            .prop_map(|(file, lines, seed)| Action::Append { file, lines, seed }),
        4 => (0..FILE_POOL.len(), any::<u8>(), any::<u8>())
            .prop_map(|(file, line, seed)| Action::Touch { file, line, seed }),
        2 => (0..FILE_POOL.len()).prop_map(|file| Action::Delete { file }),
        2 => Just(Action::JjNew),
        2 => Just(Action::JjCommit),
        1 => any::<u8>().prop_map(|seed| Action::JjDescribe { seed }),
        1 => Just(Action::JjAbandon),
        1 => Just(Action::JjSquash),
        1 => Just(Action::JjUndo),
        1 => (0..6u8).prop_map(|nth| Action::JjEditPrior { nth }),
        1 => Just(Action::JjRestoreAll),
        1 => (0..FILE_POOL.len()).prop_map(|file| Action::JjRestoreFile { file }),
        2 => (0..FILE_POOL.len(), 0..FILE_POOL.len())
            .prop_map(|(from, to)| Action::RenameFile { from, to }),
        1 => Just(Action::MoveDir),
        1 => (0..FILE_POOL.len()).prop_map(|file| Action::JjSplit { file }),
        1 => (0..8u8).prop_map(|nth| Action::JjRebase { nth }),
        1 => (0..4u8).prop_map(|nth| Action::JjNewMerge { nth }),
        1 => (0..3u8).prop_map(|style| Action::SetMarkerStyle { style }),
        2 => (0..FILE_POOL.len(), 1..8u8, any::<u8>())
            .prop_map(|(file, lines, seed)| Action::RemoteCommit { file, lines, seed }),
        2 => Just(Action::JjGitFetch),
        1 => Just(Action::JjNewOnRemote),
        2 => (0..FILE_POOL.len(), any::<bool>())
            .prop_map(|(file, on)| Action::SetExecBit { file, on }),
        2 => Just(Action::WorkspaceAdd),
        2 => (0..FILE_POOL.len(), 1..6u8, any::<u8>())
            .prop_map(|(file, lines, seed)| Action::WorkspaceOp { file, lines, seed }),
        1 => (0..FILE_POOL.len()).prop_map(|file| Action::RewriteOtherWsAncestor { file }),
        2 => (0..FILE_POOL.len(), 0..6u8)
            .prop_map(|(file, nth)| Action::RewriteWatchedWsAncestor { file, nth }),
        2 => Just(Action::WorkspaceUpdateStale),
        1 => Just(Action::WorkspaceForget),
    ]
}

/// Superset of `safe_action`: drops event batches (lossy watcher) and
/// mutates `.gitignore` (whose semantic flips are only observable at the
/// next operation, i.e. under the convergence contract).
fn lossy_action() -> impl Strategy<Value = Action> {
    prop_oneof![
        12 => safe_action(),
        1 => (0..GITIGNORE_VARIANTS.len() as u8)
            .prop_map(|variant| Action::WriteGitignore { variant }),
        1 => Just(Action::DeleteGitignore),
        2 => Just(Action::DropPendingEvents),
    ]
}

fn seeded_line(file: usize, seed: u8, i: usize) -> String {
    format!("f{file}-s{seed}-line{i}\n")
}

fn seeded_content(file: usize, lines: u8, seed: u8) -> String {
    (0..lines as usize)
        .map(|i| seeded_line(file, seed, i))
        .collect()
}

/// Snapshot of working-copy file state (excluding VCS dirs): exec bit +
/// content. The exec bit is part of the state so a chmod produces a watcher
/// event, exactly as FSEvents reports metadata-only changes.
type DiskMirror = HashMap<PathBuf, (bool, Vec<u8>)>;

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path)
        .map(|m| m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

fn read_mirror(root: &Path) -> DiskMirror {
    let mut mirror = HashMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            if name == ".jj" || name == ".git" {
                continue;
            }
            if path.is_dir() {
                stack.push(path);
            } else if let Ok(content) = std::fs::read(&path) {
                mirror.insert(path.clone(), (is_executable(&path), content));
            }
        }
    }
    mirror
}

/// Paths whose state differs between two mirrors (added, removed, changed
/// content or exec bit).
fn changed_paths(before: &DiskMirror, after: &DiskMirror) -> Vec<PathBuf> {
    let mut changed = Vec::new();
    for (path, state) in after {
        if before.get(path) != Some(state) {
            changed.push(path.clone());
        }
    }
    for path in before.keys() {
        if !after.contains_key(path) {
            changed.push(path.clone());
        }
    }
    changed.sort();
    changed
}

/// The second workspace of the harness repo: dropping the TempDir removes
/// the directory (after `jj workspace forget`, or at sequence end).
struct SecondWorkspace {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

struct Harness {
    root: PathBuf,
    worker: tokio::sync::mpsc::UnboundedSender<JjWorkerRequest>,
    config: Config,
    mirror: DiskMirror,
    /// File events not yet delivered to the worker (accumulates across
    /// actions until an op event or file-event delivery flushes it).
    pending: Vec<PathBuf>,
    /// Lossy mode: the next batch of disk changes is silently discarded,
    /// simulating FSEvents queue overflow during e.g. a checkout.
    drop_next_sync: bool,
    delivery: Delivery,
    /// Side git repo registered as the `origin` remote; RemoteCommit writes
    /// to its `main` branch, JjGitFetch imports it.
    remote: tempfile::TempDir,
    /// Second workspace ("second") of the same repo, if created.
    second_ws: Option<SecondWorkspace>,
    /// Whether the WATCHED workspace is stale (its wc-commit tree was
    /// rewritten from the second workspace). While stale, jj refuses to
    /// snapshot in the watched dir — `jj diff` errors — so oracle checks are
    /// skipped; parity is required again once `workspace update-stale`
    /// resolves. Maintained by probing after second-workspace mutations.
    watched_stale: bool,
    /// Log of executed steps, printed on failure for diagnosis.
    log: Vec<String>,
}

impl Harness {
    async fn new(root: &Path, delivery: Delivery) -> Self {
        let config = Config {
            color: false,
            ..Default::default()
        };
        // FSEvents delivers resolved (canonical) paths — mirror that, so
        // synthesized events look like what the real watcher would send.
        // (Un-canonicalized paths expose an abs_to_repo_relative limitation
        // for files deleted together with their parent directory.)
        let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        let worker = spawn_jj_worker();
        let mirror = read_mirror(&root);

        // Side "remote": a plain git repo with an initial commit on `main`.
        let remote = tempfile::TempDir::new().unwrap();
        crate::test_util::create_git_repo_in(remote.path());
        {
            // Normalize the branch name to `main` regardless of the host's
            // init.defaultBranch setting.
            let repo = git2::Repository::open(remote.path()).unwrap();
            let head = repo.head().unwrap().peel_to_commit().unwrap();
            if repo.find_branch("main", git2::BranchType::Local).is_err() {
                repo.branch("main", &head, false).unwrap();
            }
            repo.set_head("refs/heads/main").unwrap();
        }

        let mut h = Harness {
            root,
            worker,
            config,
            mirror,
            pending: Vec::new(),
            drop_next_sync: false,
            delivery,
            remote,
            second_ws: None,
            watched_stale: false,
            log: Vec::new(),
        };
        let remote_path = h.remote.path().to_str().unwrap().to_string();
        assert!(
            h.jj(&["git", "remote", "add", "origin", &remote_path]),
            "failed to add origin remote"
        );
        h.full_refresh().await;
        h
    }

    /// Commit seeded content to the remote's `main` branch via git2.
    fn remote_commit(&mut self, file: usize, lines: u8, seed: u8) {
        let repo = git2::Repository::open(self.remote.path()).unwrap();
        let parent = repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        let rel = Path::new(FILE_POOL[file]);
        let abs = self.remote.path().join(rel);
        if let Some(dir) = abs.parent() {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::fs::write(&abs, seeded_content(file, lines, seed)).unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(rel).unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let sig = repo.signature().unwrap();
        repo.commit(
            Some("refs/heads/main"),
            &sig,
            &sig,
            &format!("remote change f{file} s{seed}"),
            &tree,
            &[&parent],
        )
        .unwrap();
        self.log.push(format!(
            "remote-commit {} ({lines} lines, seed {seed})",
            FILE_POOL[file]
        ));
    }

    fn jj(&mut self, args: &[&str]) -> bool {
        let root = self.root.clone();
        self.jj_in(&root, args)
    }

    /// Like `jj` but in an explicit workspace directory (`[second]` marks
    /// second-workspace commands in the log).
    fn jj_in(&mut self, dir: &Path, args: &[&str]) -> bool {
        // ui.editor="false": any action that unexpectedly needs an editor
        // fails fast (treated as a no-op) instead of hanging the campaign.
        // The value must be quoted — a bare `false` parses as a TOML boolean,
        // which jj >= 0.42 rejects for ui.editor, failing every command.
        let output = Command::new("jj")
            .args(["--config", r#"ui.editor="false""#])
            .args(args)
            .current_dir(dir)
            .output()
            .expect("failed to spawn jj");
        let ok = output.status.success();
        self.log.push(format!(
            "jj{} {:?} -> {}{}",
            if dir == self.root { "" } else { " [second]" },
            args,
            if ok { "ok" } else { "FAILED" },
            if ok {
                String::new()
            } else {
                format!(": {}", String::from_utf8_lossy(&output.stderr))
            }
        ));
        ok
    }

    /// Tolerant stdout capture in an explicit directory: `None` on failure
    /// (e.g. a stale workspace) instead of asserting like `jj_stdout`.
    fn jj_stdout_in(&mut self, dir: &Path, args: &[&str]) -> Option<String> {
        let output = Command::new("jj")
            .args(["--config", r#"ui.editor="false""#])
            .args(args)
            .current_dir(dir)
            .output()
            .expect("failed to spawn jj");
        if output.status.success() {
            Some(String::from_utf8_lossy(&output.stdout).to_string())
        } else {
            self.log.push(format!(
                "jj [second] {:?} -> FAILED: {}",
                args,
                String::from_utf8_lossy(&output.stderr)
            ));
            None
        }
    }

    /// Re-derive `watched_stale` from jj itself: a snapshotting no-op query
    /// in the watched dir succeeds iff the workspace is healthy. Called
    /// after second-workspace mutations (same-workspace commands always
    /// update their own checkout, so those can't stale us) and before the
    /// convergence check. Any snapshot op this creates is covered by the
    /// caller's subsequent op-event delivery.
    fn probe_watched_stale(&mut self) {
        let output = Command::new("jj")
            .args(["--config", r#"ui.editor="false""#])
            .args(["log", "--no-graph", "-r", "@", "-T", "\"\""])
            .current_dir(&self.root)
            .output()
            .expect("failed to spawn jj");
        if output.status.success() {
            self.watched_stale = false;
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                stderr.contains("stale"),
                "watched-workspace probe failed for a non-stale reason: {stderr}\naction log:\n  {}",
                self.log.join("\n  "),
            );
            self.watched_stale = true;
        }
        self.log
            .push(format!("probe watched_stale={}", self.watched_stale));
    }

    /// Watched-dir stdout capture. Returns `None` — and records the
    /// staleness — when the command failed because the watched workspace is
    /// stale: staleness can arrive from second-workspace mutations through
    /// paths the flag-maintaining probes don't cover (e.g. a snapshot in
    /// the second workspace rebasing an entangled watched @), so every
    /// watched-dir query must tolerate it. Any other failure panics.
    fn jj_stdout(&mut self, args: &[&str]) -> Option<String> {
        let output = Command::new("jj")
            .args(["--config", r#"ui.editor="false""#])
            .args(args)
            .current_dir(&self.root)
            .output()
            .expect("failed to spawn jj");
        if output.status.success() {
            return Some(String::from_utf8_lossy(&output.stdout).to_string());
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("stale"),
            "jj {:?} failed: {stderr}\naction log:\n  {}",
            args,
            self.log.join("\n  "),
        );
        self.watched_stale = true;
        self.log
            .push(format!("jj {args:?} -> STALE watched workspace"));
        None
    }

    async fn full_refresh(&mut self) -> RepoStatus {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.worker
            .send(JjWorkerRequest::FullRefresh {
                repo_path: self.root.clone(),
                depth: self.config.bookmark_search_depth,
                reply: tx,
            })
            .unwrap();
        rx.await.unwrap().expect("full refresh failed")
    }

    /// Deliver accumulated file events as an IncrementalUpdate (watcher path
    /// for pure working-copy changes).
    async fn deliver_file_events(&mut self) -> Option<RepoStatus> {
        if self.pending.is_empty() {
            return None;
        }
        let paths = std::mem::take(&mut self.pending);
        self.log
            .push(format!("deliver IncrementalUpdate({paths:?})"));
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.worker
            .send(JjWorkerRequest::IncrementalUpdate {
                repo_path: self.root.clone(),
                changed_paths: paths,
                reply: tx,
            })
            .unwrap();
        Some(rx.await.unwrap().expect("incremental update failed"))
    }

    /// Deliver an op event (`.jj/op_heads` change) plus accumulated file
    /// events as a ValidateAndRefresh — the daemon path for jj commands.
    async fn deliver_op_event(&mut self) -> RepoStatus {
        let paths = std::mem::take(&mut self.pending);
        self.log
            .push(format!("deliver ValidateAndRefresh({paths:?})"));
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.worker
            .send(JjWorkerRequest::ValidateAndRefresh {
                repo_path: self.root.clone(),
                changed_paths: paths,
                depth: self.config.bookmark_search_depth,
                reply: tx,
            })
            .unwrap();
        rx.await.unwrap().expect("validate-and-refresh failed")
    }

    /// Record disk changes made since the last sync into `pending`.
    /// If a drop was requested (lossy mode), the batch is discarded instead —
    /// the mirror still advances, so those events are lost forever.
    fn sync_mirror(&mut self) {
        let after = read_mirror(&self.root);
        let changed = changed_paths(&self.mirror, &after);
        self.mirror = after;
        if self.drop_next_sync {
            self.drop_next_sync = false;
            self.log.push(format!("DROPPED events: {changed:?}"));
        } else {
            self.pending.extend(changed);
        }
    }

    /// Execute one action. Returns the worker's status if the action
    /// resulted in an event delivery.
    async fn apply(&mut self, action: &Action) -> Option<RepoStatus> {
        // jj refuses to snapshot in a stale workspace, so watched-dir
        // actions whose precondition queries use the asserting `jj_stdout`
        // are skipped up front — the commands themselves would fail as
        // no-ops anyway (the existing failure-as-noop path).
        if self.watched_stale
            && matches!(
                action,
                Action::JjSquash
                    | Action::JjEditPrior { .. }
                    | Action::JjRebase { .. }
                    | Action::JjNewMerge { .. }
                    | Action::SetMarkerStyle { .. }
            )
        {
            self.log
                .push(format!("skip {action:?}: watched workspace stale"));
            return None;
        }
        let abs = |file: &usize| self.root.join(FILE_POOL[*file]);
        match action {
            Action::Write { file, lines, seed } => {
                let path = abs(file);
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).unwrap();
                }
                std::fs::write(&path, seeded_content(*file, *lines, *seed)).unwrap();
                self.log.push(format!(
                    "write {} ({lines} lines, seed {seed})",
                    FILE_POOL[*file]
                ));
                self.sync_mirror();
                self.deliver_file_events().await
            }
            Action::Append { file, lines, seed } => {
                let path = abs(file);
                if !path.exists() {
                    return None;
                }
                let mut content = std::fs::read(&path).unwrap();
                content.extend(seeded_content(*file, *lines, *seed).into_bytes());
                std::fs::write(&path, content).unwrap();
                self.log.push(format!(
                    "append {} ({lines} lines, seed {seed})",
                    FILE_POOL[*file]
                ));
                self.sync_mirror();
                self.deliver_file_events().await
            }
            Action::Touch { file, line, seed } => {
                let path = abs(file);
                if !path.exists() {
                    return None;
                }
                let content = std::fs::read_to_string(&path).unwrap();
                let mut lines: Vec<&str> = content.lines().collect();
                if lines.is_empty() {
                    return None;
                }
                let idx = *line as usize % lines.len();
                let replacement = format!("f{file}-touched-s{seed}");
                lines[idx] = &replacement;
                std::fs::write(&path, lines.join("\n") + "\n").unwrap();
                self.log.push(format!(
                    "touch {} line {idx} (seed {seed})",
                    FILE_POOL[*file]
                ));
                self.sync_mirror();
                self.deliver_file_events().await
            }
            Action::Delete { file } => {
                let path = abs(file);
                if !path.exists() {
                    return None;
                }
                std::fs::remove_file(&path).unwrap();
                self.log.push(format!("delete {}", FILE_POOL[*file]));
                self.sync_mirror();
                self.deliver_file_events().await
            }
            Action::JjNew => self.run_jj_op(&["new"]).await,
            Action::JjCommit => self.run_jj_op(&["commit", "-m", "commit"]).await,
            Action::JjDescribe { seed } => {
                let msg = format!("desc-{seed}");
                self.run_jj_op(&["describe", "-m", &msg]).await
            }
            Action::JjAbandon => self.run_jj_op(&["abandon"]).await,
            Action::JjSquash => {
                // Squashing into the root commit fails; only squash when @
                // has a non-root parent. Cheap check: does @- have a parent?
                let out = self.jj_stdout(&[
                    "log",
                    "--no-graph",
                    "-r",
                    "@-",
                    "-T",
                    "if(root, \"root\", \"ok\")",
                ])?;
                if out.contains("root") {
                    return None;
                }
                // -m avoids the editor jj opens to combine two descriptions.
                self.run_jj_op(&["squash", "-m", "squashed"]).await
            }
            Action::JjUndo => {
                if !self.jj(&["undo"]) {
                    return None;
                }
                // Undoing the workspace-creating op restores to root() and
                // leaves the workspace with no working-copy commit — a
                // degenerate state the daemon (correctly) errors on and that
                // we don't model. Detect it and redo.
                if !self.jj(&["log", "--no-graph", "-r", "@", "-T", "\"\""]) {
                    assert!(self.jj(&["redo"]), "jj redo failed after bad undo");
                }
                self.sync_mirror();
                Some(self.deliver_op_event().await)
            }
            Action::JjEditPrior { nth } => {
                // Pick the nth visible mutable change (excluding @).
                let out = self.jj_stdout(&[
                    "log",
                    "--no-graph",
                    "-r",
                    "mutable() ~ @",
                    "-T",
                    "change_id ++ \"\\n\"",
                ])?;
                let ids: Vec<&str> = out.lines().filter(|l| !l.is_empty()).collect();
                if ids.is_empty() {
                    return None;
                }
                let id = ids[*nth as usize % ids.len()].to_string();
                self.run_jj_op(&["edit", &id]).await
            }
            Action::JjRestoreAll => self.run_jj_op(&["restore"]).await,
            Action::JjRestoreFile { file } => self.run_jj_op(&["restore", FILE_POOL[*file]]).await,
            Action::RenameFile { from, to } => {
                if from == to {
                    return None;
                }
                let src = abs(from);
                let dst = abs(to);
                if !src.is_file() {
                    return None;
                }
                if let Some(parent) = dst.parent() {
                    std::fs::create_dir_all(parent).unwrap();
                }
                std::fs::rename(&src, &dst).unwrap();
                self.log
                    .push(format!("rename {} -> {}", FILE_POOL[*from], FILE_POOL[*to]));
                self.sync_mirror();
                self.deliver_file_events().await
            }
            Action::MoveDir => {
                let d1 = self.root.join("dir");
                let d2 = self.root.join("dir2");
                let (src, dst, label) = if d1.is_dir() && !d2.exists() {
                    (d1, d2, "dir -> dir2")
                } else if d2.is_dir() && !d1.exists() {
                    (d2, d1, "dir2 -> dir")
                } else {
                    return None;
                };
                std::fs::rename(&src, &dst).unwrap();
                self.log.push(format!("move-dir {label}"));
                self.sync_mirror();
                self.deliver_file_events().await
            }
            Action::JjSplit { file } => {
                self.run_jj_op(&["split", "-m", "split-out", "--", FILE_POOL[*file]])
                    .await
            }
            Action::JjRebase { nth } => {
                let out = self.jj_stdout(&[
                    "log",
                    "--no-graph",
                    "-r",
                    "all() ~ @",
                    "-T",
                    "change_id ++ \"\\n\"",
                ])?;
                let ids: Vec<&str> = out.lines().filter(|l| !l.is_empty()).collect();
                if ids.is_empty() {
                    return None;
                }
                let id = ids[*nth as usize % ids.len()].to_string();
                self.run_jj_op(&["rebase", "-r", "@", "-d", &id]).await
            }
            Action::JjNewMerge { nth } => {
                // Other heads exist after edit-prior + new branched history.
                let out = self.jj_stdout(&[
                    "log",
                    "--no-graph",
                    "-r",
                    "heads(all()) ~ ::@",
                    "-T",
                    "change_id ++ \"\\n\"",
                ])?;
                let ids: Vec<&str> = out.lines().filter(|l| !l.is_empty()).collect();
                if ids.is_empty() {
                    return None;
                }
                let id = ids[*nth as usize % ids.len()].to_string();
                self.run_jj_op(&["new", "@", &id]).await
            }
            Action::SetMarkerStyle { style } => {
                // jj does not rematerialize existing on-disk conflicts when
                // the style changes, so only toggle while @ is conflict-free.
                let conflicted = !self
                    .jj_stdout(&["log", "--no-graph", "-r", "@ & conflicts()", "-T", "\"c\""])?
                    .is_empty();
                if conflicted {
                    self.log
                        .push("skip set-marker-style: @ is conflicted".to_string());
                    return None;
                }
                let name = ["diff", "snapshot", "git"][*style as usize % 3];
                self.log.push(format!("set conflict-marker-style {name}"));
                self.jj(&["config", "set", "--repo", "ui.conflict-marker-style", name]);
                // Config changes create no jj operation and no watcher event.
                None
            }
            Action::RemoteCommit { file, lines, seed } => {
                self.remote_commit(*file, *lines, *seed);
                // Nothing observable locally until a fetch.
                None
            }
            Action::JjGitFetch => self.run_jj_op(&["git", "fetch"]).await,
            Action::JjNewOnRemote => {
                // Requires a prior fetch to have imported main@origin.
                if !self.jj(&["log", "--no-graph", "-r", "main@origin", "-T", "\"\""]) {
                    return None;
                }
                self.run_jj_op(&["new", "main@origin"]).await
            }
            Action::WriteGitignore { variant } => {
                let content = GITIGNORE_VARIANTS[*variant as usize % GITIGNORE_VARIANTS.len()];
                std::fs::write(self.root.join(".gitignore"), content).unwrap();
                self.log
                    .push(format!("write .gitignore {:?}", content.replace('\n', " ")));
                self.sync_mirror();
                self.deliver_file_events().await
            }
            Action::DeleteGitignore => {
                let path = self.root.join(".gitignore");
                if !path.exists() {
                    return None;
                }
                std::fs::remove_file(&path).unwrap();
                self.log.push("delete .gitignore".to_string());
                self.sync_mirror();
                self.deliver_file_events().await
            }
            Action::DropPendingEvents => {
                if self.delivery == Delivery::Lossy {
                    self.log
                        .push("arm event drop for next disk change".to_string());
                    self.drop_next_sync = true;
                }
                None
            }
            Action::SetExecBit { file, on } => {
                use std::os::unix::fs::PermissionsExt as _;
                let path = abs(file);
                if !path.is_file() {
                    return None;
                }
                let mut perms = std::fs::metadata(&path).unwrap().permissions();
                let mode = perms.mode();
                let new_mode = if *on { mode | 0o111 } else { mode & !0o111 };
                if new_mode == mode {
                    return None;
                }
                perms.set_mode(new_mode);
                std::fs::set_permissions(&path, perms).unwrap();
                self.log.push(format!(
                    "chmod {} {}",
                    if *on { "+x" } else { "-x" },
                    FILE_POOL[*file]
                ));
                self.sync_mirror();
                self.deliver_file_events().await
            }
            Action::WorkspaceAdd => {
                if self.second_ws.is_some() {
                    return None;
                }
                let dir = tempfile::TempDir::with_prefix("jj-second-ws-").unwrap();
                let root = dir.path().join("second");
                let root_str = root.to_str().unwrap().to_string();
                if !self.jj(&["workspace", "add", "--name", "second", &root_str]) {
                    return None;
                }
                let root = root.canonicalize().unwrap_or(root);
                self.second_ws = Some(SecondWorkspace { _dir: dir, root });
                self.sync_mirror();
                Some(self.deliver_op_event().await)
            }
            Action::WorkspaceOp { file, lines, seed } => {
                let ws_root = self.second_ws.as_ref().map(|w| w.root.clone())?;
                let path = ws_root.join(FILE_POOL[*file]);
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).unwrap();
                }
                std::fs::write(&path, seeded_content(*file, *lines, *seed)).unwrap();
                self.log.push(format!(
                    "workspace-op write {} in second ({lines} lines, seed {seed})",
                    FILE_POOL[*file]
                ));
                // Snapshot in the second workspace — the foreign operation.
                // If that workspace is stale (a watched-side action rewrote
                // its @), recover it first like a user would.
                let ok = self.jj_in(&ws_root, &["status"])
                    || (self.jj_in(&ws_root, &["workspace", "update-stale"])
                        && self.jj_in(&ws_root, &["status"]));
                if !ok {
                    return None;
                }
                // Even a snapshot in the second workspace can stale the
                // watched one: when the watched @ is a descendant of second@
                // (via JjRebase/JjNewMerge onto it), amending second@
                // auto-rebases the watched wc commit with a tree change.
                self.probe_watched_stale();
                self.sync_mirror();
                Some(self.deliver_op_event().await)
            }
            Action::RewriteOtherWsAncestor { file } => {
                self.second_ws.as_ref()?;
                // Tree-changing rewrite of the second workspace's @ from the
                // watched workspace. The watched side stays healthy —
                // same-workspace commands always update their own checkout —
                // while the second workspace goes stale (unless the restore
                // was "Nothing changed").
                self.run_jj_op(&[
                    "restore",
                    "--from",
                    "@",
                    "--into",
                    "second@",
                    "--",
                    FILE_POOL[*file],
                ])
                .await
            }
            Action::RewriteWatchedWsAncestor { file, nth } => {
                let ws_root = self.second_ws.as_ref().map(|w| w.root.clone())?;
                // Candidates: mutable commits on the watched line, excluding
                // the second workspace's own ancestry (usually `default@`
                // plus watched-side stack). --ignore-working-copy keeps both
                // the query and the rewrite working when second is stale.
                let out = self.jj_stdout_in(
                    &ws_root,
                    &[
                        "--ignore-working-copy",
                        "log",
                        "--no-graph",
                        "-r",
                        "(mutable() & ::default@) ~ ::@",
                        "-T",
                        "change_id ++ \"\\n\"",
                    ],
                )?;
                let ids: Vec<&str> = out.lines().filter(|l| !l.is_empty()).collect();
                if ids.is_empty() {
                    return None;
                }
                let id = ids[*nth as usize % ids.len()].to_string();
                if !self.jj_in(
                    &ws_root,
                    &[
                        "--ignore-working-copy",
                        "restore",
                        "--from",
                        "@",
                        "--into",
                        &id,
                        "--",
                        FILE_POOL[*file],
                    ],
                ) {
                    return None;
                }
                // Staleness only arises when the watched wc-commit *tree*
                // actually changed (identical-content restores are "Nothing
                // changed"; jj auto-heals tree-preserving rewrites) — let jj
                // itself be the authority.
                self.probe_watched_stale();
                self.sync_mirror();
                Some(self.deliver_op_event().await)
            }
            Action::WorkspaceUpdateStale => {
                // Runs unconditionally: recovery in a healthy workspace is a
                // no-op with exit 0 ("Attempted recovery, but the working
                // copy is not stale").
                if !self.jj(&["workspace", "update-stale"]) {
                    return None;
                }
                self.watched_stale = false;
                // update-stale rewrites disk files (checkout, no user edit).
                self.sync_mirror();
                Some(self.deliver_op_event().await)
            }
            Action::WorkspaceForget => {
                self.second_ws.as_ref()?;
                let status = self.run_jj_op(&["workspace", "forget", "second"]).await;
                if status.is_some() {
                    // TempDir drop removes the on-disk workspace.
                    self.second_ws = None;
                }
                status
            }
        }
    }

    /// Run a jj command; on success, sync the mirror (the op may have
    /// rewritten working-copy files) and deliver an op event.
    async fn run_jj_op(&mut self, args: &[&str]) -> Option<RepoStatus> {
        if !self.jj(args) {
            // Command legitimately refused (e.g. undo at repo start) — treat
            // as a no-op rather than a generator bug.
            return None;
        }
        self.sync_mirror();
        Some(self.deliver_op_event().await)
    }

    /// Compare the worker's current stats against `jj diff --stat`.
    ///
    /// Running the oracle snapshots the working copy (creating an op); we
    /// deliver that op event afterwards so the worker stays in sync, exactly
    /// as the real watcher would observe it.
    async fn check_oracle(&mut self, worker_status: &RepoStatus) -> Result<(), String> {
        let Some(stat) = self.jj_stdout(&["diff", "--stat"]) else {
            // The oracle itself hit the stale error: the watched workspace
            // went stale under us (jj_stdout has recorded it). Parity is
            // deferred to the forced update-stale + convergence check.
            return Ok(());
        };
        let (files, added, removed) = parse_diff_stat_summary(&stat);
        let got = (
            worker_status.file_mad_count_working_tree,
            worker_status.lines_added_working_tree,
            worker_status.lines_removed_working_tree,
        );
        // Sync the snapshot op created by the oracle run.
        self.sync_mirror();
        self.deliver_op_event().await;
        if got != (files, added, removed) {
            return Err(format!(
                "worker ({}f, +{}, -{}) != jj diff --stat ({files}f, +{added}, -{removed})\n\
                 jj output:\n{stat}\naction log:\n  {}",
                got.0,
                got.1,
                got.2,
                self.log.join("\n  "),
            ));
        }
        Ok(())
    }
}

/// Run one generated sequence.
///
/// - Perfect delivery: the worker must match `jj diff --stat` after **every**
///   action — exact live parity.
/// - Lossy delivery: events may be lost, and files the daemon has *never*
///   heard about cannot be conjured without a full disk walk. The contract is
///   weaker but still hard: the **next jj operation** (whose snapshot brings
///   lost changes into the store) must restore exact parity.
async fn run_sequence(actions: &[Action], delivery: Delivery) -> Result<(), String> {
    let dir = create_jj_repo();
    // SetMarkerStyle runs `jj config set --repo`, which creates an external
    // per-repo config dir; remove it when the sequence ends (even on panic).
    let _config_cleanup = crate::test_util::RepoConfigCleanup::new(dir.path());
    let mut h = Harness::new(dir.path(), delivery).await;

    for action in actions {
        let status = h.apply(action).await;
        // While the watched workspace is stale, `jj diff` itself errors
        // ("The working copy is stale") — there is no oracle to compare
        // against, so parity is deferred to the update-stale recovery.
        if delivery == Delivery::Perfect
            && !h.watched_stale
            && let Some(status) = status
        {
            h.check_oracle(&status).await?;
        }
    }

    // A sequence may end inside a stale window (a second-workspace
    // mutation rewrote the watched @). Probe definitively — the flag can
    // lag when the very last staleness-inducing op had no watched-dir
    // follow-up — then force the recovery the staleness contract is
    // defined around: parity MUST hold once `workspace update-stale`
    // resolves, which the convergence check below then verifies.
    h.drop_next_sync = false;
    h.probe_watched_stale();
    if h.watched_stale {
        let status = h.apply(&Action::WorkspaceUpdateStale).await;
        assert!(
            status.is_some() && !h.watched_stale,
            "workspace update-stale failed to recover the watched workspace:\n  {}",
            h.log.join("\n  ")
        );
    }

    // Convergence check. Run a jj operation and deliver its op event — in
    // lossy mode this is the moment the engine is required to re-anchor to
    // the store; in perfect mode it must simply stay correct. `describe` is
    // used (not `status`) because it always writes a new operation: a
    // no-op snapshot advances nothing, and the healing contract is "the
    // next *operation*", which is also what produces a watcher event.
    // The healing contract depends on this operation actually happening — a
    // silent failure here (e.g. a config error) makes the whole lossy
    // property vacuous, so fail loudly instead of no-opping.
    assert!(
        h.jj(&["describe", "-m", "convergence-check"]),
        "convergence-check describe failed: {}",
        h.log.last().map(String::as_str).unwrap_or("")
    );
    h.sync_mirror();
    let status = h.deliver_op_event().await;
    h.check_oracle(&status).await?;
    Ok(())
}

/// The case currently executing, for the `DIFF_PROP_FAILURE_FILE` hook.
static CURRENT_CASE: Mutex<String> = Mutex::new(String::new());

fn run_case(actions: Vec<Action>, delivery: Delivery) -> Result<(), TestCaseError> {
    // Soak-campaign diagnostic: when `DIFF_PROP_FAILURE_FILE` is set, write
    // the failing case (oracle mismatch or panic, wherever it fires) to that
    // file. Long background runs routinely lose captured test output, and a
    // panic mid-shrink otherwise leaves nothing to replay.
    if let Ok(path) = std::env::var("DIFF_PROP_FAILURE_FILE") {
        *CURRENT_CASE.lock().unwrap() = format!("delivery={delivery:?}\nactions={actions:#?}");
        static HOOK: std::sync::Once = std::sync::Once::new();
        HOOK.call_once(move || {
            let prev = std::panic::take_hook();
            std::panic::set_hook(Box::new(move |info| {
                let case = CURRENT_CASE.lock().unwrap();
                let _ = std::fs::write(&path, format!("PANIC: {info}\n\n{case}"));
                prev(info);
            }));
        });
    }
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(run_sequence(&actions, delivery)).map_err(|e| {
        if let Ok(path) = std::env::var("DIFF_PROP_FAILURE_FILE") {
            let _ = std::fs::write(
                &path,
                format!("delivery={delivery:?}\nactions={actions:#?}\n\n{e}"),
            );
        }
        TestCaseError::fail(e)
    })
}

/// Sequence length range; `DIFF_PROP_SEQ_LEN` sets the upper bound
/// (default 10). Longer sequences catch drift that only compounds.
fn seq_len() -> std::ops::Range<usize> {
    let max: usize = std::env::var("DIFF_PROP_SEQ_LEN")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10);
    3..max.max(4)
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: std::env::var("DIFF_PROP_CASES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(8),
        max_shrink_iters: 200,
        .. ProptestConfig::default()
    })]

    /// CI property: with a lossless watcher, the incremental engine matches
    /// `jj diff --stat` after every action.
    #[test]
    fn prop_incremental_matches_cli_perfect_events(
        actions in proptest::collection::vec(safe_action(), seq_len())
    ) {
        run_case(actions, Delivery::Perfect)?;
    }

    /// Lossy property: even when event batches are dropped (FSEvents
    /// overflow, dir-level coalescing) and `.gitignore` semantics flip, the
    /// next jj operation must restore exact `jj diff --stat` parity — drift
    /// is bounded, never permanent.
    #[test]
    fn prop_incremental_converges_lossy_events(
        actions in proptest::collection::vec(lossy_action(), seq_len())
    ) {
        run_case(actions, Delivery::Lossy)?;
    }
}

/// Deterministic replays of shrunk failure sequences — permanent regression
/// tests. Add new entries as the property tests find bugs.
mod regressions {
    use super::*;

    fn replay(actions: &[Action], delivery: Delivery) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        if let Err(msg) = rt.block_on(run_sequence(actions, delivery)) {
            panic!("{msg}");
        }
    }

    /// Abandon with dropped file events: the parent tree is unchanged, so
    /// historically stale base_file_stats survived forever. The commit-tree
    /// check in ValidateAndRefresh must rebuild them. (Same root cause as
    /// `repro_stale_base_after_abandon_without_events` in jj.rs.)
    #[test]
    fn lossy_abandon_converges_after_op() {
        replay(
            &[
                Action::Write {
                    file: 0,
                    lines: 3,
                    seed: 1,
                },
                Action::JjNew,
                Action::Write {
                    file: 1,
                    lines: 2,
                    seed: 2,
                },
                Action::DropPendingEvents,
                Action::JjAbandon,
            ],
            Delivery::Lossy,
        );
    }

    /// Directory move of a tracked file right after `jj new` — found by the
    /// 40-case campaign (flaky there; deterministic replay pins it).
    #[test]
    fn movedir_of_tracked_file_after_new() {
        replay(
            &[
                Action::Write {
                    file: 2, // dir/c.txt
                    lines: 1,
                    seed: 0,
                },
                Action::Write {
                    file: 0,
                    lines: 1,
                    seed: 0,
                },
                Action::JjNew,
                Action::MoveDir,
            ],
            Delivery::Perfect,
        );
    }

    /// Soak-found (flaky at soak volume; deterministic replay pins it):
    /// fetch/new-on-remote/undo churn, then append + rename. The failing
    /// runs reported the rename as an unpaired delete+add with a stale
    /// pre-append base for the source file.
    #[test]
    fn soak_fetch_undo_then_rename_after_append() {
        replay(
            &[
                Action::Write {
                    file: 5,
                    lines: 2,
                    seed: 10,
                },
                Action::JjGitFetch,
                Action::JjNew,
                Action::JjNewOnRemote,
                Action::JjUndo,
                Action::Append {
                    file: 5,
                    lines: 1,
                    seed: 0,
                },
                Action::RenameFile { from: 5, to: 0 },
            ],
            Delivery::Perfect,
        );
    }

    /// Soak-found companion case: squash + gitignore flip + fetch, then an
    /// in-place edit, then undo.
    #[test]
    fn soak_squash_gitignore_fetch_touch_undo() {
        replay(
            &[
                Action::Write {
                    file: 3,
                    lines: 1,
                    seed: 0,
                },
                Action::JjSquash,
                Action::WriteGitignore { variant: 1 },
                Action::JjGitFetch,
                Action::Touch {
                    file: 3,
                    line: 0,
                    seed: 0,
                },
                Action::JjUndo,
            ],
            Delivery::Perfect,
        );
    }

    /// Soak-found: rename a file, then re-create the source with content
    /// identical to the snapshotted version. The re-created source
    /// invalidates the rename premise — the target must revert to a pure
    /// add (jj shows `a.txt | 1 +`, source unchanged), not stay paired as
    /// a zero-diff rename.
    #[test]
    fn soak2_rename_then_recreate_source() {
        replay(
            &[
                Action::Write {
                    file: 5,
                    lines: 1,
                    seed: 0,
                },
                Action::JjNew,
                Action::RenameFile { from: 5, to: 0 },
                Action::Write {
                    file: 5,
                    lines: 1,
                    seed: 0,
                },
            ],
            Delivery::Perfect,
        );
    }

    /// Soak-found: zero-diff directory move split into delete+add after
    /// abandon → undo churn on top of a merge (jj shows
    /// `{dir => dir2}/c.txt | 0`).
    #[test]
    fn soak2_movedir_after_merge_abandon_undo() {
        replay(
            &[
                Action::Write {
                    file: 2,
                    lines: 1,
                    seed: 45,
                },
                Action::JjGitFetch,
                Action::JjNewOnRemote,
                Action::JjNewMerge { nth: 1 },
                Action::Write {
                    file: 5,
                    lines: 5,
                    seed: 40,
                },
                Action::MoveDir,
                Action::JjAbandon,
                Action::JjUndo,
            ],
            Delivery::Perfect,
        );
    }

    /// Soak-found (lossy): edits on both sides of a merge parent — the
    /// worker must diff against the conflict-materialized merged parent
    /// content like jj does (jj reported +4/-4, worker +1/-1).
    #[test]
    fn soak2_merge_conflicted_parent_touch() {
        replay(
            &[
                Action::Write {
                    file: 0,
                    lines: 1,
                    seed: 0,
                },
                Action::JjGitFetch,
                Action::JjNewOnRemote,
                Action::Write {
                    file: 0,
                    lines: 1,
                    seed: 1,
                },
                Action::JjNewMerge { nth: 0 },
                Action::JjNewMerge { nth: 0 },
                Action::Touch {
                    file: 0,
                    line: 6,
                    seed: 0,
                },
            ],
            Delivery::Lossy,
        );
    }

    /// Soak-3 (lossy): a file whose tree value changed but whose contents
    /// diff to zero lines must still count as one changed file (jj shows
    /// `dir/sub/d.txt | 0` alongside the conflicted `a.txt | 8 ++++----`).
    #[test]
    fn soak3_zero_line_change_still_counts_file() {
        replay(
            &[
                Action::Write {
                    file: 0,
                    lines: 1,
                    seed: 0,
                },
                Action::JjGitFetch,
                Action::JjNewOnRemote,
                Action::Write {
                    file: 3,
                    lines: 1,
                    seed: 0,
                },
                Action::Write {
                    file: 0,
                    lines: 1,
                    seed: 1,
                },
                Action::JjRebase { nth: 7 },
                Action::JjNewMerge { nth: 0 },
                Action::JjNewOnRemote,
                Action::Write {
                    file: 3,
                    lines: 1,
                    seed: 1,
                },
                Action::JjNewMerge { nth: 0 },
                Action::Touch {
                    file: 0,
                    line: 6,
                    seed: 0,
                },
            ],
            Delivery::Lossy,
        );
    }

    /// Soak-3: rename ONTO a path that already exists in the parent tree
    /// (arrived via a fetched merge side). jj pairs it as a rename and the
    /// old target baseline vanishes: `{e.rs => dir/sub/d.txt} | 0`.
    #[test]
    fn soak3_rename_onto_existing_path() {
        replay(
            &[
                Action::RemoteCommit {
                    file: 3,
                    lines: 1,
                    seed: 0,
                },
                Action::Write {
                    file: 4,
                    lines: 9,
                    seed: 128,
                },
                Action::JjNew,
                Action::JjGitFetch,
                Action::JjNewMerge { nth: 1 },
                Action::RenameFile { from: 4, to: 3 },
            ],
            Delivery::Perfect,
        );
    }

    /// Soak-3 (inverted): move of a file whose snapshotted content shares
    /// nothing with its parent-tree content. jj's copy detection compares
    /// the *diff endpoints* (source@parent vs target@wc) and does NOT pair
    /// dissimilar content: `dir/c.txt | 1 -` + `dir2/c.txt | 1 +`.
    #[test]
    fn soak3_movedir_dissimilar_content_not_paired() {
        replay(
            &[
                Action::Write {
                    file: 2,
                    lines: 1,
                    seed: 0,
                },
                Action::JjNew,
                Action::Write {
                    file: 2,
                    lines: 1,
                    seed: 1,
                },
                Action::MoveDir,
            ],
            Delivery::Perfect,
        );
    }

    /// Soak-14: a paired directory move followed by `jj restore` of the
    /// source. The restored source is unmodified vs the *merged* parent but
    /// a Modification vs one individual parent, so gix keeps the pairing as
    /// a COPY record: jj shows `{dir => dir2}/c.txt | 0`. The worker voided
    /// the pairing on source visibility and reported a plain add (+4).
    #[test]
    fn soak14_restored_source_keeps_pairing_as_copy() {
        replay(
            &[
                Action::RemoteCommit {
                    file: 2,
                    lines: 4,
                    seed: 252,
                },
                Action::JjGitFetch,
                Action::JjNewOnRemote,
                Action::Write {
                    file: 2,
                    lines: 4,
                    seed: 176,
                },
                Action::WorkspaceAdd,
                Action::JjNewOnRemote,
                Action::WorkspaceOp {
                    file: 5,
                    lines: 3,
                    seed: 224,
                },
                Action::WorkspaceForget,
                Action::JjNew,
                Action::JjNew,
                Action::JjNew,
                Action::JjNewOnRemote,
                Action::JjNewMerge { nth: 2 },
                Action::WorkspaceAdd,
                Action::JjNewOnRemote,
                Action::JjNewMerge { nth: 0 },
                Action::MoveDir,
                Action::JjRestoreFile { file: 2 },
                Action::JjEditPrior { nth: 4 },
            ],
            Delivery::Perfect,
        );
    }

    /// Soak-13 F1: soak12's poison scenario plus deleting the poisoned
    /// target. With the add gone, gix finds no records at the next diff and
    /// the previously-dropped source delete resurfaces: jj shows
    /// `dir/sub/d.txt | 1 -`; the worker saw nothing.
    #[test]
    fn soak13_poisoned_delete_resurfaces_when_target_deleted() {
        replay(
            &[
                Action::RemoteCommit {
                    file: 3,
                    lines: 1,
                    seed: 0,
                },
                Action::Append {
                    file: 0,
                    lines: 1,
                    seed: 244,
                },
                Action::Write {
                    file: 5,
                    lines: 5,
                    seed: 61,
                },
                Action::JjGitFetch,
                Action::JjNewOnRemote,
                Action::WorkspaceAdd,
                Action::JjNewMerge { nth: 0 },
                Action::RenameFile { from: 3, to: 4 },
                Action::Delete { file: 4 },
            ],
            Delivery::Perfect,
        );
    }

    /// Soak-13 F2: a conflicted file renamed onto another path, then
    /// `ui.conflict-marker-style` flips to git before the next op. jj's
    /// stat recomputes marker-text line counts with the CURRENT style;
    /// base entries baked at rebuild time with the old style must be
    /// re-derived (jj: `a.txt | 8 +++++++-` + `e.rs | 6 ------`).
    #[test]
    fn soak13_marker_style_change_rederives_base_entries() {
        replay(
            &[
                Action::Write {
                    file: 4,
                    lines: 1,
                    seed: 0,
                },
                Action::Write {
                    file: 0,
                    lines: 1,
                    seed: 0,
                },
                Action::RemoteCommit {
                    file: 4,
                    lines: 1,
                    seed: 1,
                },
                Action::JjGitFetch,
                Action::JjNewMerge { nth: 0 },
                Action::RenameFile { from: 4, to: 0 },
                Action::SetMarkerStyle { style: 2 },
            ],
            Delivery::Perfect,
        );
    }

    /// Soak-12: merge with a freshly added workspace's WC as the second
    /// parent, then a rename whose source exists in both parents. jj's
    /// duplicate-record poisoning shows the target as a plain add with the
    /// delete dropped (`e.rs | 1 +` only); the worker paired it as a
    /// zero-line rename.
    #[test]
    fn soak12_rename_after_workspace_merge_poisons() {
        replay(
            &[
                Action::RemoteCommit {
                    file: 3,
                    lines: 1,
                    seed: 0,
                },
                Action::Append {
                    file: 0,
                    lines: 1,
                    seed: 122,
                },
                Action::Write {
                    file: 5,
                    lines: 5,
                    seed: 61,
                },
                Action::JjGitFetch,
                Action::JjNewOnRemote,
                Action::WorkspaceAdd,
                Action::JjNewMerge { nth: 0 },
                Action::RenameFile { from: 3, to: 4 },
            ],
            Delivery::Perfect,
        );
    }

    /// Soak-11: heavy split/merge/rebase churn, then `rename b.txt -> x.log`
    /// where the target path exists (conflicted) in the merged parent. jj
    /// pairs `{b.txt => x.log} | 0`; the worker reported two unpaired files
    /// with conflict-materialization-sized counts.
    #[test]
    fn soak11_rename_onto_conflicted_path_pairs() {
        replay(
            &[
                Action::Write {
                    file: 0,
                    lines: 1,
                    seed: 0,
                },
                Action::Write {
                    file: 1,
                    lines: 1,
                    seed: 0,
                },
                Action::RemoteCommit {
                    file: 2,
                    lines: 1,
                    seed: 1,
                },
                Action::JjNewMerge { nth: 0 },
                Action::JjNewOnRemote,
                Action::JjSplit { file: 2 },
                Action::JjRebase { nth: 3 },
                Action::RemoteCommit {
                    file: 5,
                    lines: 5,
                    seed: 164,
                },
                Action::JjNewMerge { nth: 3 },
                Action::JjSplit { file: 1 },
                Action::JjSplit { file: 2 },
                Action::JjSplit { file: 5 },
                Action::Write {
                    file: 2,
                    lines: 5,
                    seed: 92,
                },
                Action::JjNewOnRemote,
                Action::JjEditPrior { nth: 0 },
                Action::JjNew,
                Action::JjNewMerge { nth: 1 },
                Action::JjEditPrior { nth: 4 },
                Action::JjUndo,
                Action::Write {
                    file: 5,
                    lines: 9,
                    seed: 250,
                },
                Action::Write {
                    file: 0,
                    lines: 3,
                    seed: 43,
                },
                Action::JjGitFetch,
                Action::RenameFile { from: 1, to: 3 },
                Action::JjNew,
                Action::Append {
                    file: 5,
                    lines: 1,
                    seed: 230,
                },
                Action::Write {
                    file: 0,
                    lines: 8,
                    seed: 105,
                },
                Action::JjRebase { nth: 1 },
                Action::RemoteCommit {
                    file: 5,
                    lines: 2,
                    seed: 82,
                },
                Action::JjEditPrior { nth: 5 },
                Action::JjNewMerge { nth: 2 },
                Action::JjGitFetch,
                Action::Append {
                    file: 1,
                    lines: 4,
                    seed: 50,
                },
                Action::JjNewMerge { nth: 3 },
                Action::RenameFile { from: 1, to: 5 },
            ],
            Delivery::Perfect,
        );
    }

    /// Soak-10: triple-merge churn, then a touch inside a conflicted file.
    /// jj shows only `dir/sub/d.txt | 2 +-`; the worker reported one extra
    /// (phantom) file.
    #[test]
    fn soak10_touch_in_conflict_after_triple_merge_no_phantom() {
        replay(
            &[
                Action::JjNew,
                Action::Write {
                    file: 3,
                    lines: 3,
                    seed: 0,
                },
                Action::JjEditPrior { nth: 0 },
                Action::JjUndo,
                Action::Write {
                    file: 5,
                    lines: 1,
                    seed: 0,
                },
                Action::RemoteCommit {
                    file: 0,
                    lines: 1,
                    seed: 0,
                },
                Action::JjDescribe { seed: 167 },
                Action::JjNew,
                Action::Append {
                    file: 5,
                    lines: 1,
                    seed: 230,
                },
                Action::JjRebase { nth: 1 },
                Action::RemoteCommit {
                    file: 5,
                    lines: 2,
                    seed: 82,
                },
                Action::JjEditPrior { nth: 5 },
                Action::JjNewMerge { nth: 2 },
                Action::JjGitFetch,
                Action::JjNewMerge { nth: 2 },
                Action::Write {
                    file: 3,
                    lines: 1,
                    seed: 38,
                },
                Action::JjNewMerge { nth: 3 },
                Action::Touch {
                    file: 3,
                    line: 51,
                    seed: 78,
                },
            ],
            Delivery::Perfect,
        );
    }

    /// Soak-9: double-merge/split churn leaves a.txt with a parent-vs-WC
    /// *value* difference whose contents diff to zero lines; jj reports
    /// `a.txt | 0` alongside the ordinary d.txt edit, the worker missed
    /// the zero-line file.
    #[test]
    fn soak9_zero_line_file_alongside_edit_after_double_merge() {
        replay(
            &[
                Action::RemoteCommit {
                    file: 3,
                    lines: 1,
                    seed: 0,
                },
                Action::Write {
                    file: 3,
                    lines: 1,
                    seed: 1,
                },
                Action::JjGitFetch,
                Action::JjNewMerge { nth: 0 },
                Action::JjNewMerge { nth: 0 },
                Action::JjSplit { file: 1 },
                Action::JjSplit { file: 5 },
                Action::Write {
                    file: 0,
                    lines: 3,
                    seed: 43,
                },
                Action::JjGitFetch,
                Action::Write {
                    file: 0,
                    lines: 8,
                    seed: 105,
                },
                Action::JjNewMerge { nth: 2 },
                Action::JjNewOnRemote,
                Action::Write {
                    file: 0,
                    lines: 6,
                    seed: 252,
                },
                Action::JjNewMerge { nth: 0 },
                Action::Write {
                    file: 3,
                    lines: 3,
                    seed: 244,
                },
            ],
            Delivery::Perfect,
        );
    }

    /// Soak-8: `ui.conflict-marker-style` changed between snapshots creates
    /// no op and no watcher event, but jj's very next `diff --stat`
    /// materializes conflicted values with the NEW style — the incremental
    /// path must re-resolve the style instead of using the one cached at
    /// the last full refresh (off-by-one marker-text line: +1/-13 vs
    /// jj's +1/-14).
    #[test]
    fn soak8_marker_style_change_between_snapshots() {
        replay(
            &[
                Action::Write {
                    file: 4,
                    lines: 6,
                    seed: 195,
                },
                Action::JjCommit,
                Action::JjNew,
                Action::SetMarkerStyle { style: 2 },
                Action::Write {
                    file: 4,
                    lines: 3,
                    seed: 105,
                },
                Action::JjNew,
                Action::JjNew,
                Action::JjEditPrior { nth: 2 },
                Action::JjSquash,
                Action::RenameFile { from: 4, to: 0 },
                Action::JjSquash,
                Action::JjEditPrior { nth: 0 },
                Action::JjNew,
                Action::JjCommit,
                Action::JjNew,
                Action::Write {
                    file: 0,
                    lines: 10,
                    seed: 231,
                },
                Action::JjCommit,
                Action::Write {
                    file: 0,
                    lines: 4,
                    seed: 43,
                },
                Action::JjEditPrior { nth: 3 },
                Action::Write {
                    file: 4,
                    lines: 9,
                    seed: 133,
                },
                Action::SetMarkerStyle { style: 0 },
                Action::Write {
                    file: 4,
                    lines: 1,
                    seed: 73,
                },
                Action::Append {
                    file: 4,
                    lines: 1,
                    seed: 212,
                },
            ],
            Delivery::Perfect,
        );
    }

    /// Soak-8: a snapshotted rename target rewritten with content
    /// dissimilar to the source voids the pairing — gix re-detects from
    /// scratch on every diff, so jj shows a plain `a.txt | 1 +` AND the
    /// previously-suppressed source delete resurfaces (`b.txt | 1 -`).
    #[test]
    fn soak8_rewritten_target_unpairs_and_resurrects_delete() {
        replay(
            &[
                Action::Write {
                    file: 1,
                    lines: 1,
                    seed: 0,
                },
                Action::Delete { file: 1 },
                Action::JjUndo,
                Action::JjNew,
                Action::RenameFile { from: 1, to: 0 },
                Action::Write {
                    file: 0,
                    lines: 1,
                    seed: 0,
                },
            ],
            Delivery::Perfect,
        );
    }

    /// Soak-7 (found during the synthetic-state refactor baseline): a
    /// rename source re-created with *different* content is a modified
    /// file, but gix matches copy targets against the source's NEW content
    /// (`Change::id()` of a Modification is the post-image blob) — seed1 vs
    /// the target's seed0 content is dissimilar, so jj shows a plain
    /// `a.txt | 1 +` + `x.log | 1 +-`, not a copy pairing.
    #[test]
    fn soak7_recreated_source_different_content_plain_add() {
        replay(
            &[
                Action::Write {
                    file: 5,
                    lines: 1,
                    seed: 0,
                },
                Action::JjNew,
                Action::RenameFile { from: 5, to: 0 },
                Action::Write {
                    file: 5,
                    lines: 1,
                    seed: 1,
                },
            ],
            Delivery::Perfect,
        );
    }

    /// Soak-6: gix *copy* detection (CopySource::FromSetOfModifiedFiles):
    /// after `jj restore` re-creates a rename source, appending to it makes
    /// it a modified file — and gix then pairs the previously-plain-added
    /// target as a COPY of it: jj shows `dir/c.txt | 2 ++` plus
    /// `{dir => dir2}/c.txt | 0` (both files counted, copy target at zero).
    #[test]
    fn soak6_copy_detection_after_restore_and_append() {
        replay(
            &[
                Action::Write {
                    file: 0,
                    lines: 1,
                    seed: 0,
                },
                Action::Delete { file: 4 },
                Action::JjSplit { file: 1 },
                Action::RemoteCommit {
                    file: 2,
                    lines: 1,
                    seed: 175,
                },
                Action::Write {
                    file: 2,
                    lines: 3,
                    seed: 38,
                },
                Action::JjCommit,
                Action::JjRebase { nth: 6 },
                Action::JjNewMerge { nth: 2 },
                Action::JjNew,
                Action::JjGitFetch,
                Action::JjSquash,
                Action::MoveDir,
                Action::JjRestoreFile { file: 2 },
                Action::Append {
                    file: 2,
                    lines: 2,
                    seed: 202,
                },
            ],
            Delivery::Perfect,
        );
    }

    /// Soak-6: split/restore/rebase/merge churn, then a directory move that
    /// jj reports as a plain delete + add (`dir/c.txt | 1 -` +
    /// `dir2/c.txt | 1 +`), while the worker pairs it into one file.
    #[test]
    fn soak6_movedir_after_rebase_merge_not_paired() {
        replay(
            &[
                Action::Write {
                    file: 0,
                    lines: 1,
                    seed: 0,
                },
                Action::JjNew,
                Action::JjSplit { file: 0 },
                Action::JjRestoreAll,
                Action::Write {
                    file: 2,
                    lines: 1,
                    seed: 0,
                },
                Action::JjNew,
                Action::Write {
                    file: 2,
                    lines: 1,
                    seed: 1,
                },
                Action::JjRebase { nth: 1 },
                Action::JjNewMerge { nth: 0 },
                Action::MoveDir,
            ],
            Delivery::Perfect,
        );
    }

    /// Soak-6 (lossy): a rename source re-created as an *ignored untracked*
    /// file is invisible to jj — the rename pairing must survive
    /// (`{dir => dir2}/sub/d.txt | 0`), unlike a visible re-creation which
    /// voids it (soak2_rename_then_recreate_source).
    #[test]
    fn soak6_ignored_source_recreation_keeps_rename() {
        replay(
            &[
                Action::Write {
                    file: 3,
                    lines: 1,
                    seed: 0,
                },
                Action::JjNew,
                Action::WriteGitignore { variant: 1 },
                Action::MoveDir,
                Action::JjGitFetch,
                Action::Write {
                    file: 3,
                    lines: 1,
                    seed: 0,
                },
            ],
            Delivery::Lossy,
        );
    }

    /// Soak-5: directory move of a *conflicted* file (the merge sides wrote
    /// 6-line vs 1-line content). gix's per-parent copy detection compares
    /// each parent's plain side blob against the moved marker text — ~0%
    /// similar — so jj pairs nothing: `dir/c.txt | 12 -` + `dir2/c.txt | 12 +`.
    /// Pairing against the merged-parent materialization (the marker text
    /// itself, 100% similar) is wrong.
    #[test]
    fn soak5_movedir_conflicted_file_not_paired() {
        replay(
            &[
                Action::RemoteCommit {
                    file: 2,
                    lines: 1,
                    seed: 0,
                },
                Action::JjGitFetch,
                Action::Write {
                    file: 0,
                    lines: 1,
                    seed: 0,
                },
                Action::Append {
                    file: 0,
                    lines: 1,
                    seed: 173,
                },
                Action::Write {
                    file: 2,
                    lines: 6,
                    seed: 168,
                },
                Action::JjNewMerge { nth: 2 },
                Action::MoveDir,
            ],
            Delivery::Perfect,
        );
    }

    /// Soak-5 companion: same shape with 1-line sides (seed1 vs seed0);
    /// jj shows `dir/c.txt | 7 -` + `dir2/c.txt | 7 +` (marker-text lines),
    /// no pairing.
    #[test]
    fn soak5_movedir_conflicted_file_not_paired_small() {
        replay(
            &[
                Action::RemoteCommit {
                    file: 2,
                    lines: 1,
                    seed: 0,
                },
                Action::JjGitFetch,
                Action::Write {
                    file: 0,
                    lines: 1,
                    seed: 0,
                },
                Action::Write {
                    file: 0,
                    lines: 1,
                    seed: 0,
                },
                Action::Write {
                    file: 2,
                    lines: 1,
                    seed: 1,
                },
                Action::JjNewMerge { nth: 0 },
                Action::MoveDir,
            ],
            Delivery::Perfect,
        );
    }

    /// Soak-4: after edit-prior/abandon/commit/merge/rebase churn, a file
    /// whose parent and WC-commit *values* differ but whose contents diff
    /// to zero lines (`dir/c.txt | 0` in jj) must survive the incremental
    /// re-diff after the re-anchor — a disk event with content equal to the
    /// parent materialization must not mask the standing zero-line entry.
    #[test]
    fn soak4_zero_line_entry_survives_incremental_rediff() {
        replay(
            &[
                Action::RemoteCommit {
                    file: 2,
                    lines: 1,
                    seed: 0,
                },
                Action::JjGitFetch,
                Action::Write {
                    file: 2,
                    lines: 1,
                    seed: 0,
                },
                Action::JjNew,
                Action::Append {
                    file: 2,
                    lines: 1,
                    seed: 3,
                },
                Action::JjEditPrior { nth: 4 },
                Action::JjAbandon,
                Action::JjNew,
                Action::JjCommit,
                Action::Write {
                    file: 2,
                    lines: 4,
                    seed: 22,
                },
                Action::JjNewOnRemote,
                Action::JjNewMerge { nth: 3 },
                Action::Write {
                    file: 5,
                    lines: 9,
                    seed: 66,
                },
                Action::JjCommit,
                Action::JjNew,
                Action::Touch {
                    file: 5,
                    line: 251,
                    seed: 212,
                },
                Action::JjRebase { nth: 6 },
            ],
            Delivery::Perfect,
        );
    }

    /// Soak-4: directory move under a merge where *both* parents contain
    /// the source path. Each per-parent copy detection emits a record for
    /// the same target; jj-lib discards duplicate records (CopyRecords
    /// poisons them), so jj shows the target as a plain add and silently
    /// drops the source's delete: `dir2/c.txt | 1 +` only.
    #[test]
    fn soak4_movedir_source_in_both_merge_parents() {
        replay(
            &[
                Action::RemoteCommit {
                    file: 2,
                    lines: 1,
                    seed: 0,
                },
                Action::JjGitFetch,
                Action::Write {
                    file: 0,
                    lines: 1,
                    seed: 0,
                },
                Action::Write {
                    file: 0,
                    lines: 1,
                    seed: 0,
                },
                Action::Write {
                    file: 2,
                    lines: 1,
                    seed: 0,
                },
                Action::JjNewMerge { nth: 0 },
                Action::MoveDir,
            ],
            Delivery::Perfect,
        );
    }

    /// Rename then edit the target across snapshots (perfect mode): the
    /// overlay must diff the target against the rename *source's* parent
    /// content, matching jj's `{a => b} | 2 +-` fuzzy-rename stat.
    #[test]
    fn rename_then_edit_target() {
        replay(
            &[
                Action::Write {
                    file: 0, // a.txt
                    lines: 11,
                    seed: 7,
                },
                Action::JjNew,
                Action::RenameFile { from: 0, to: 1 }, // a.txt -> b.txt
                Action::Touch {
                    file: 1,
                    line: 5,
                    seed: 9,
                },
            ],
            Delivery::Perfect,
        );
    }

    /// Exec-bit-only change parity: chmod +x on a committed file is one
    /// changed file with 0 line changes (`e.rs | 0`), chmod -x reverts it
    /// to no change; a flip layered on a content edit keeps the line stats.
    #[test]
    fn exec_bit_only_change_parity() {
        replay(
            &[
                Action::Write {
                    file: 4, // e.rs
                    lines: 3,
                    seed: 1,
                },
                Action::JjCommit,
                Action::SetExecBit { file: 4, on: true },
                Action::SetExecBit { file: 4, on: false },
                Action::SetExecBit { file: 4, on: true },
                Action::Append {
                    file: 4,
                    lines: 2,
                    seed: 7,
                },
                Action::JjNew,
                Action::SetExecBit { file: 4, on: false },
            ],
            Delivery::Perfect,
        );
    }

    /// Staleness contract: a second-workspace rewrite of the watched @'s
    /// tree makes the watched workspace stale (oracle checks impossible —
    /// `jj diff` errors); `jj workspace update-stale` rewrites disk files
    /// with no user edit and parity MUST hold immediately after.
    #[test]
    fn stale_then_update_stale_restores_parity() {
        replay(
            &[
                Action::Write {
                    file: 0,
                    lines: 3,
                    seed: 1,
                },
                Action::JjCommit,
                Action::WorkspaceAdd,
                Action::Write {
                    file: 0,
                    lines: 2,
                    seed: 9,
                },
                Action::WorkspaceOp {
                    file: 1,
                    lines: 2,
                    seed: 3,
                },
                // second@ still holds the pre-commit a.txt content; restoring
                // it into default@ changes the watched wc-commit tree.
                Action::RewriteWatchedWsAncestor { file: 0, nth: 0 },
                // Edits during the stale window still reach the worker.
                Action::Append {
                    file: 0,
                    lines: 1,
                    seed: 5,
                },
                Action::WorkspaceUpdateStale,
                Action::Write {
                    file: 2,
                    lines: 1,
                    seed: 4,
                },
            ],
            Delivery::Perfect,
        );
    }

    /// Workspace-entanglement staleness: once the watched @ is a merge
    /// child of second@, a mere *snapshot* in the second workspace amends
    /// second@ and auto-rebases the watched wc commit — staling the watched
    /// workspace with no watched-side action at all. The harness must
    /// detect it (probe after every second-workspace mutation) so
    /// watched-dir precondition queries don't assert on the stale error,
    /// and parity must return after update-stale.
    #[test]
    fn workspace_op_stales_entangled_watched_ws() {
        replay(
            &[
                Action::Write {
                    file: 0,
                    lines: 1,
                    seed: 0,
                },
                Action::JjCommit,
                Action::WorkspaceAdd,
                // heads(all()) ~ ::@ == {second@} → merge @ with second@.
                Action::JjNewMerge { nth: 0 },
                Action::WorkspaceOp {
                    file: 1,
                    lines: 2,
                    seed: 7,
                },
                // Pre-fix this panicked in the jj_stdout conflict probe
                // ("The working copy is stale"); post-fix it must be a
                // graceful stale-skip.
                Action::SetMarkerStyle { style: 0 },
                Action::WorkspaceUpdateStale,
                Action::Write {
                    file: 2,
                    lines: 1,
                    seed: 3,
                },
            ],
            Delivery::Perfect,
        );
    }

    /// Rename AND edit within one un-snapshotted window (lossy): only jj's
    /// copy records can pair them (fuzzy rename); the convergence full
    /// refresh must use them.
    #[test]
    fn lossy_rename_and_edit_converges_via_copy_records() {
        replay(
            &[
                Action::Write {
                    file: 0,
                    lines: 11,
                    seed: 7,
                },
                Action::JjNew,
                Action::RenameFile { from: 0, to: 1 },
                Action::Touch {
                    file: 1,
                    line: 5,
                    seed: 9,
                },
            ],
            Delivery::Lossy,
        );
    }
}
