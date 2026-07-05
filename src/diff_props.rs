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

/// Snapshot of working-copy file contents (excluding VCS dirs).
type DiskMirror = HashMap<PathBuf, Vec<u8>>;

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
                mirror.insert(path, content);
            }
        }
    }
    mirror
}

/// Paths whose content differs between two mirrors (added, removed, changed).
fn changed_paths(before: &DiskMirror, after: &DiskMirror) -> Vec<PathBuf> {
    let mut changed = Vec::new();
    for (path, content) in after {
        if before.get(path) != Some(content) {
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
        // ui.editor=false: any action that unexpectedly needs an editor
        // fails fast (treated as a no-op) instead of hanging the campaign.
        let output = Command::new("jj")
            .args(["--config", "ui.editor=false"])
            .args(args)
            .current_dir(&self.root)
            .output()
            .expect("failed to spawn jj");
        let ok = output.status.success();
        self.log.push(format!(
            "jj {:?} -> {}{}",
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

    fn jj_stdout(&self, args: &[&str]) -> String {
        let output = Command::new("jj")
            .args(["--config", "ui.editor=false"])
            .args(args)
            .current_dir(&self.root)
            .output()
            .expect("failed to spawn jj");
        assert!(
            output.status.success(),
            "jj {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).to_string()
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
                ]);
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
                ]);
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
                ]);
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
                ]);
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
                    .jj_stdout(&["log", "--no-graph", "-r", "@ & conflicts()", "-T", "\"c\""])
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
        let stat = self.jj_stdout(&["diff", "--stat"]);
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
        if delivery == Delivery::Perfect
            && let Some(status) = status
        {
            h.check_oracle(&status).await?;
        }
    }

    // Convergence check. Run a jj operation and deliver its op event — in
    // lossy mode this is the moment the engine is required to re-anchor to
    // the store; in perfect mode it must simply stay correct. `describe` is
    // used (not `status`) because it always writes a new operation: a
    // no-op snapshot advances nothing, and the healing contract is "the
    // next *operation*", which is also what produces a watcher event.
    h.drop_next_sync = false;
    h.jj(&["describe", "-m", "convergence-check"]);
    h.sync_mirror();
    let status = h.deliver_op_event().await;
    h.check_oracle(&status).await?;
    Ok(())
}

fn run_case(actions: Vec<Action>, delivery: Delivery) -> Result<(), TestCaseError> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(run_sequence(&actions, delivery))
        .map_err(TestCaseError::fail)
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
