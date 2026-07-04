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
const FILE_POOL: &[&str] = &["a.txt", "b.txt", "dir/c.txt", "dir/sub/d.txt", "e.rs"];

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
    /// Lossy mode only: drop all not-yet-delivered file events, simulating
    /// FSEvents queue overflow. In Perfect mode this is skipped.
    DropPendingEvents,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    Perfect,
    Lossy,
}

/// Strategy for the safe action set: text files, trailing newlines, no
/// conflicts-by-construction is NOT guaranteed (jj merges can conflict via
/// undo/edit interleavings are excluded), and no event drops.
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
    ]
}

/// Superset of `safe_action` that also drops event batches (lossy watcher).
fn lossy_action() -> impl Strategy<Value = Action> {
    prop_oneof![
        9 => safe_action(),
        1 => Just(Action::DropPendingEvents),
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
        let mut h = Harness {
            root,
            worker,
            config,
            mirror,
            pending: Vec::new(),
            drop_next_sync: false,
            delivery,
            log: Vec::new(),
        };
        h.full_refresh().await;
        h
    }

    fn jj(&mut self, args: &[&str]) -> bool {
        let output = Command::new("jj")
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
                self.run_jj_op(&["squash"]).await
            }
            Action::JjUndo => {
                // Undoing the repo-init op leaves the workspace with no
                // working-copy commit — a degenerate state we don't model.
                // Only undo when at least one op exists beyond init.
                let ops =
                    self.jj_stdout(&["op", "log", "--no-graph", "--limit", "3", "-T", "\"op\\n\""]);
                if ops.lines().count() < 3 {
                    return None;
                }
                self.run_jj_op(&["undo"]).await
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
    let mut h = Harness::new(dir.path(), delivery).await;

    for action in actions {
        let status = h.apply(action).await;
        if delivery == Delivery::Perfect
            && let Some(status) = status
        {
            h.check_oracle(&status).await?;
        }
    }

    // Convergence check. Run a jj op (snapshot) and deliver its op event —
    // in lossy mode this is the moment the engine is required to re-anchor
    // to the store; in perfect mode it must simply stay correct.
    h.drop_next_sync = false;
    h.jj(&["status"]);
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
        actions in proptest::collection::vec(safe_action(), 3..10)
    ) {
        run_case(actions, Delivery::Perfect)?;
    }

    /// Lossy property: even when event batches are dropped (FSEvents
    /// overflow, dir-level coalescing), the next jj operation must restore
    /// exact `jj diff --stat` parity — drift is bounded, never permanent.
    #[test]
    fn prop_incremental_converges_lossy_events(
        actions in proptest::collection::vec(lossy_action(), 3..10)
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
}
