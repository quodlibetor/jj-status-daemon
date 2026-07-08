use anyhow::{Context, Result};
use futures::StreamExt;
use jj_lib::backend::CommitId;
use jj_lib::config::{ConfigLayer, ConfigSource, StackedConfig};
use jj_lib::conflicts::ConflictMarkerStyle;
use jj_lib::diff::DiffHunkKind;
use jj_lib::diff_presentation::{LineCompareMode, diff_by_line};
use jj_lib::fileset::FilesetAliasesMap;
use jj_lib::hex_util::encode_reverse_hex;
use jj_lib::matchers::EverythingMatcher;
use jj_lib::object_id::ObjectId;
use jj_lib::ref_name::{RemoteName, WorkspaceName};
use jj_lib::repo::{Repo, StoreFactories};
use jj_lib::revset::{
    self, RevsetAliasesMap, RevsetDiagnostics, RevsetExtensions, RevsetParseContext,
    RevsetWorkspaceContext, SymbolResolver,
};
use jj_lib::settings::UserSettings;
use jj_lib::time_util::DatePatternContext;
use jj_lib::workspace::{Workspace, default_working_copy_factories};
use serde::Deserialize as _;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, OnceLock};

use tokio::sync::mpsc;

use crate::config::Config;
use crate::template::{Bookmark, RepoStatus, TrackingStatus};

/// Create minimal UserSettings for read-only operations. The settings are
/// constant, so they are built once and cloned (UserSettings is cheap to
/// clone) instead of re-parsing the default config on every repo load.
fn create_user_settings() -> Result<UserSettings> {
    static SETTINGS: OnceLock<UserSettings> = OnceLock::new();
    if let Some(settings) = SETTINGS.get() {
        return Ok(settings.clone());
    }
    let mut config = StackedConfig::with_defaults();
    let mut user_layer = ConfigLayer::empty(ConfigSource::User);
    user_layer
        .set_value("user.name", "vcs-status-daemon")
        .context("set user.name")?;
    user_layer
        .set_value("user.email", "vcs-status-daemon@localhost")
        .context("set user.email")?;
    config.add_layer(user_layer);
    let settings = UserSettings::from_config(config).context("create UserSettings")?;
    Ok(SETTINGS.get_or_init(|| settings).clone())
}

/// Read and parse a TOML config file, cached by (mtime, size) so repeated
/// refreshes don't re-read and re-parse unchanged files. Returns `None` for
/// missing or unparseable files (parse failures are cached too).
fn cached_config_table(path: &Path) -> Option<Arc<toml::Table>> {
    type Key = (std::time::SystemTime, u64);
    type Cache = HashMap<PathBuf, (Key, Option<Arc<toml::Table>>)>;
    static CACHE: LazyLock<Mutex<Cache>> = LazyLock::new(Default::default);

    let meta = std::fs::metadata(path).ok()?;
    let key = (meta.modified().ok()?, meta.len());
    let mut cache = CACHE.lock().unwrap();
    if let Some((cached_key, table)) = cache.get(path)
        && *cached_key == key
    {
        return table.clone();
    }
    let table = std::fs::read_to_string(path)
        .ok()
        .and_then(|content| content.parse::<toml::Table>().ok())
        .map(Arc::new);
    cache.insert(path.to_path_buf(), (key, table.clone()));
    table
}

/// Materialize a tree value to the byte content `jj diff` would compare:
/// plain file bytes, symlink targets as text, and conflicts rendered with
/// conflict markers exactly as jj materializes them in the working copy.
///
/// Returns `None` for absent values and non-content values (submodules,
/// unreadable entries) — matching jj, which diffs those as empty.
async fn materialized_content(
    store: &Arc<jj_lib::store::Store>,
    path: &jj_lib::repo_path::RepoPath,
    value: jj_lib::merge::MergedTreeValue,
    labels: &jj_lib::conflict_labels::ConflictLabels,
    marker_style: ConflictMarkerStyle,
) -> Option<Vec<u8>> {
    let materialized = jj_lib::conflicts::materialize_tree_value(store, path, value, labels)
        .await
        .ok()?;
    materialized_bytes(store, path, materialized, marker_style).await
}

/// Convert an already-materialized tree value to bytes (see
/// [`materialized_content`]).
async fn materialized_bytes(
    store: &Arc<jj_lib::store::Store>,
    path: &jj_lib::repo_path::RepoPath,
    materialized: jj_lib::conflicts::MaterializedTreeValue,
    marker_style: ConflictMarkerStyle,
) -> Option<Vec<u8>> {
    use jj_lib::conflicts::{
        ConflictMaterializeOptions, MaterializedTreeValue, choose_materialized_conflict_marker_len,
        materialize_merge_result_to_bytes,
    };

    match materialized {
        MaterializedTreeValue::Absent => None,
        MaterializedTreeValue::AccessDenied(_) => None,
        MaterializedTreeValue::File(mut file) => file.read_all(path).await.ok(),
        MaterializedTreeValue::Symlink { target, .. } => Some(target.into_bytes()),
        MaterializedTreeValue::FileConflict(file) => {
            // Same recipe as jj's working-copy checkout, so the materialized
            // bytes match both `jj diff` output and the conflicted file jj
            // writes to disk.
            let marker_len = choose_materialized_conflict_marker_len(&file.contents);
            let options = ConflictMaterializeOptions {
                marker_style,
                marker_len: Some(marker_len),
                merge: store.merge_options().clone(),
            };
            Some(materialize_merge_result_to_bytes(&file.contents, &file.labels, &options).into())
        }
        MaterializedTreeValue::OtherConflict { id, labels } => {
            Some(id.describe(&labels).into_bytes())
        }
        MaterializedTreeValue::GitSubmodule(_) => None,
        MaterializedTreeValue::Tree(_) => None,
    }
}

/// Resolve the user's `ui.conflict-marker-style` the way jj would for this
/// repo: built-in default, then user config files, then repo config
/// (`.jj/repo/config.toml`), later sources overriding earlier ones.
///
/// jj rematerializes on-disk conflicts only when trees change, so a file
/// written before a config change may briefly carry old-style markers; we
/// read the current config at refresh time, which matches what jj will
/// write from now on.
fn conflict_marker_style_for_repo(repo_path: &Path) -> ConflictMarkerStyle {
    let mut style = ConflictMarkerStyle::Diff; // jj's built-in default

    // User-level config paths (superset of `load_user_revset_aliases`),
    // then repo-level config, which has the highest precedence in jj.
    let mut config_paths: Vec<PathBuf> = [
        std::env::var("JJ_CONFIG").ok().map(PathBuf::from),
        dirs::home_dir().map(|d| d.join(".jjconfig.toml")),
    ]
    .into_iter()
    .flatten()
    .collect();
    config_paths.extend(jj_config_roots().into_iter().map(|d| d.join("config.toml")));
    config_paths.push(repo_config_path(repo_path));

    for path in config_paths {
        let Some(table) = cached_config_table(&path) else {
            continue;
        };
        let Some(value) = table
            .get("ui")
            .and_then(|ui| ui.get("conflict-marker-style"))
        else {
            continue;
        };
        match ConflictMarkerStyle::deserialize(value.clone()) {
            Ok(parsed) => style = parsed,
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "invalid ui.conflict-marker-style in jj config, ignoring"
                );
            }
        }
    }

    style
}

/// Path to the repo-level jj config.
///
/// Modern jj stores it outside the repo, keyed by `.jj/repo/config-id`:
/// `<config dir>/jj/repos/<config-id>/config.toml`. Older repos used
/// `.jj/repo/config.toml` directly. In a secondary workspace `.jj/repo` is
/// a file containing the path of the primary repo's store directory.
fn repo_config_path(repo_path: &Path) -> PathBuf {
    let mut repo_dir = repo_path.join(".jj").join("repo");
    if repo_dir.is_file()
        && let Ok(target) = std::fs::read_to_string(&repo_dir)
    {
        repo_dir = PathBuf::from(target.trim());
    }
    if let Ok(config_id) = std::fs::read_to_string(repo_dir.join("config-id")) {
        let config_id = config_id.trim();
        if !config_id.is_empty() && config_id.chars().all(|c| c.is_ascii_hexdigit()) {
            for root in jj_config_roots() {
                let candidate = root.join("repos").join(config_id).join("config.toml");
                if candidate.exists() {
                    return candidate;
                }
            }
        }
    }
    repo_dir.join("config.toml")
}

/// Candidate jj config root directories (containing `config.toml`, `repos/`).
/// jj checks the platform config dir and, on macOS where they differ,
/// `~/.config` as well.
pub(crate) fn jj_config_roots() -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Some(d) = dirs::config_dir() {
        roots.push(d.join("jj"));
    }
    if let Some(home) = dirs::home_dir() {
        let xdg = home.join(".config").join("jj");
        if !roots.contains(&xdg) {
            roots.push(xdg);
        }
    }
    roots
}

/// Check if content looks binary by scanning for null bytes in the first 8KB.
fn is_binary(content: &[u8]) -> bool {
    let check_len = content.len().min(8192);
    content[..check_len].contains(&0)
}

/// Count lines the way `diff --stat` does: a trailing fragment without a
/// final newline still counts as a line.
fn count_lines(content: &[u8]) -> u32 {
    if content.is_empty() {
        return 0;
    }
    let newlines = bytecount::count(content, b'\n') as u32;
    if content.ends_with(b"\n") {
        newlines
    } else {
        newlines + 1
    }
}

/// Classification of a file change.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FileChangeKind {
    #[default]
    Modified,
    Added,
    Deleted,
    /// Git-only: file exists on disk but is not in the index or HEAD.
    Untracked,
}

/// Per-file diff stats for incremental overlay tracking.
#[derive(Clone, Debug, Default)]
pub struct FileDiffStats {
    pub lines_added: u32,
    pub lines_removed: u32,
    pub kind: FileChangeKind,
    /// Fingerprint of the file content (added: new content; deleted: old
    /// content). Used to pair equal-content Added/Deleted entries as renames,
    /// mirroring git's exact-rename detection. `None` for empty content
    /// (git excludes empty files from rename detection) and for kinds where
    /// pairing does not apply.
    pub content_hash: Option<u64>,
    /// For entries produced by jj copy records: the repo-relative source
    /// path this file was renamed/copied from. The incremental single-file
    /// diff uses it to compare disk content against the *source's* parent
    /// content, as jj does.
    pub renamed_from: Option<String>,
}

/// FNV-1a over content — a stable fingerprint for exact-rename pairing.
fn content_fingerprint(content: &[u8]) -> Option<u64> {
    if content.is_empty() {
        return None;
    }
    let mut hash: u64 = 0xcbf29ce484222325;
    for &b in content {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    Some(hash)
}

/// Similarity score in [0, 1] between two contents, approximating gix's
/// rename-tracking metric (gix-diff `rewrites/tracker.rs`:
/// `(old_len - removed_bytes) / max(old_len, new_len)` — the fraction of
/// the larger side covered by source bytes retained across the diff).
/// jj's git backend pairs delete+add as a rename at >= 50% similarity
/// (`GitBackend::get_copy_records` configures
/// `gix::diff::Rewrites { percentage: Some(0.5), .. }`).
fn content_similarity(before: &[u8], after: &[u8]) -> f32 {
    if before.is_empty() || after.is_empty() {
        // gix sets track_empty: false — empty files never pair.
        return 0.0;
    }
    if before == after {
        return 1.0;
    }
    if is_binary(before) || is_binary(after) {
        return 0.0;
    }
    let diff = diff_by_line([before, after], &LineCompareMode::Exact);
    let removed: usize = diff
        .hunks()
        .filter(|hunk| hunk.kind == DiffHunkKind::Different)
        .map(|hunk| hunk.contents[0].len())
        .sum();
    (before.len() - removed) as f32 / before.len().max(after.len()) as f32
}

/// Aggregated diff statistics including per-category file counts.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DiffCounts {
    pub file_mad_count: u32,
    pub lines_added: u32,
    pub lines_removed: u32,
    pub files_modified: u32,
    pub files_added: u32,
    pub files_deleted: u32,
    pub files_untracked: u32,
}

/// Retained jj-lib state for incremental working copy diffs.
///
/// This is NOT Send (jj-lib internals use RefCell/OnceCell), so it must live
/// on a blocking thread that can call jj-lib async APIs.
pub struct JjRepoState {
    store: Arc<jj_lib::store::Store>,
    parent_tree: jj_lib::merged_tree::MergedTree,
    /// Tree IDs of the parent tree, used to detect baseline changes without full diff.
    parent_tree_ids: jj_lib::merge::Merge<jj_lib::backend::TreeId>,
    /// The working-copy commit's own tree — the last snapshot, i.e. the
    /// authority on which paths are *tracked* and what content they last
    /// had. Its IDs detect ops that rewrite the WC commit without changing
    /// the parent (snapshot, abandon, edit-to-sibling, restore): the base
    /// stats must then be rebuilt from the store — watcher events alone
    /// cannot be relied on to repair them.
    commit_tree: jj_lib::merged_tree::MergedTree,
    /// The individual parent trees as gix sees them: the FIRST term of each
    /// parent's root-tree merge (`GitBackend::read_tree_for_commit` uses
    /// `tree.first()`), so a conflicted parent contributes that term's
    /// blobs, never marker text or the merged view. jj's copy detection
    /// runs per parent: under a merge a path that exists in the *merged*
    /// parent tree can still be a rename target (an addition relative to
    /// the parent that owned the source), and a source present in multiple
    /// parents produces duplicate copy records that jj-lib discards — all
    /// of it depends on which parents contain a path.
    parent_trees: Vec<jj_lib::merged_tree::MergedTree>,
    /// Operation ID at the time this state was built. Tree ID comparison
    /// alone misses A→B→A sequences (e.g. `jj abandon` snapshots a dirty
    /// file into @ and then discards it — both trees end up as they
    /// started, but the op rewrote the working copy on disk). When the op
    /// advanced with unchanged trees, overlay entries must be re-diffed.
    op_id: jj_lib::op_store::OperationId,
    /// The user's current `ui.conflict-marker-style`. Set at full refresh
    /// and re-resolved on every incremental batch: a config change creates
    /// no op and no watcher event, yet jj's very next `diff --stat`
    /// materializes conflicted values with the new style.
    conflict_marker_style: ConflictMarkerStyle,
    /// Ignore rules for this repo. Files that are untracked AND ignored
    /// are invisible to `jj diff` (jj never auto-tracks them), so they
    /// must not produce overlay entries — otherwise a file written before
    /// a `.gitignore` started covering it leaves a permanent phantom Add.
    ignore_filter: crate::watcher::IgnoreFilter,
    /// Paths conflicted in the WC commit tree, collected once per full
    /// refresh (O(conflicts): `MergedTree::conflicts` recurses only into
    /// conflicted subtrees). A snapshot that actually *writes* re-resolves
    /// the whole tree merge, so conflicted paths WITHOUT their own disk
    /// changes can still get new stored values — content-identical but
    /// shape-different: jj reports `file | 0`.
    conflicted_paths: Vec<jj_lib::repo_path::RepoPathBuf>,
    /// Per-file stats from the last full jj-lib diff (parent_tree vs commit.tree()).
    base_file_stats: HashMap<String, FileDiffStats>,
    /// Overlay: per-file stats computed from disk reads for working copy files.
    /// `Some(stats)` overrides a base entry; `None` means file matches parent (remove from diff).
    overlay: HashMap<String, Option<FileDiffStats>>,
    /// Repo root (canonical) for converting absolute paths to repo-relative.
    repo_root: PathBuf,
    /// The full RepoStatus minus diff stats, so we can re-render without full reload.
    base_status: RepoStatus,
}

/// Compute aggregate diff stats by merging base per-file stats with an overlay.
///
/// - Base entries not in overlay: counted as-is.
/// - Base entries with `Some(stats)` in overlay: overlay replaces base.
/// - Base entries with `None` in overlay: file reverted to parent, excluded.
/// - Overlay entries not in base: new files created after snapshot.
pub fn aggregate_overlay_stats(
    base: &HashMap<String, FileDiffStats>,
    overlay: &HashMap<String, Option<FileDiffStats>>,
) -> DiffCounts {
    let mut counts = DiffCounts::default();

    // Every entry counts as a changed file, even with 0 added/removed lines
    // (empty files, binary files, exec-bit changes) — matching `diff --stat`,
    // which reports e.g. "1 file changed, 0 insertions(+), 0 deletions(-)".
    // Entries are only created when a file actually differs, so no-change
    // paths must be represented by absence (or `None` in the overlay).
    fn tally(counts: &mut DiffCounts, stats: &FileDiffStats) {
        if stats.kind == FileChangeKind::Untracked {
            counts.files_untracked += 1;
            return;
        }
        counts.file_mad_count += 1;
        counts.lines_added += stats.lines_added;
        counts.lines_removed += stats.lines_removed;
        match stats.kind {
            FileChangeKind::Modified => counts.files_modified += 1,
            FileChangeKind::Added => counts.files_added += 1,
            FileChangeKind::Deleted => counts.files_deleted += 1,
            FileChangeKind::Untracked => unreachable!(),
        }
    }

    // Process base entries, checking for overlay overrides
    for (path, stats) in base {
        match overlay.get(path) {
            Some(Some(overlay_stats)) => {
                tally(&mut counts, overlay_stats);
            }
            Some(None) => {
                // File reverted to parent — excluded from diff
            }
            None => {
                tally(&mut counts, stats);
            }
        }
    }

    // Process overlay entries not in base (new files created after snapshot)
    for (path, entry) in overlay {
        if base.contains_key(path) {
            continue;
        }
        if let Some(stats) = entry {
            tally(&mut counts, stats);
        }
    }

    // Rename/copy pairing is NOT done here: jj's verdict depends on
    // per-parent tree contents (gix copy detection), which plain
    // Added/Deleted hash equality cannot reproduce — it both misses fuzzy
    // renames and pairs cases jj splits. Pairing is resolved into the
    // overlay entries themselves by `apply_incremental_paths`.

    counts
}

/// Like `aggregate_overlay_stats` but groups results by directory.
///
/// Returns a vec of `(directory, IncrementalDiffStats)` sorted by directory name.
/// The root directory is represented as `"."`.
pub fn aggregate_overlay_stats_by_dir(
    base: &HashMap<String, FileDiffStats>,
    overlay: &HashMap<String, Option<FileDiffStats>>,
) -> Vec<(String, crate::protocol::IncrementalDiffStats)> {
    use std::collections::BTreeMap;

    let mut dirs: BTreeMap<String, crate::protocol::IncrementalDiffStats> = BTreeMap::new();

    fn dir_of(path: &str) -> &str {
        match path.rfind('/') {
            Some(i) => &path[..i],
            None => ".",
        }
    }

    fn tally_file(
        acc: &mut crate::protocol::IncrementalDiffStats,
        stats: &FileDiffStats,
        is_base: bool,
        is_overlay: bool,
    ) {
        if is_base {
            acc.base_files += 1;
        }
        if is_overlay {
            acc.overlay_entries += 1;
        }
        if stats.kind != FileChangeKind::Untracked {
            acc.files_changed += 1;
            acc.lines_added += stats.lines_added;
            acc.lines_removed += stats.lines_removed;
        }
    }

    // Process base entries
    for (path, stats) in base {
        let dir = dir_of(path);
        let acc = dirs.entry(dir.to_string()).or_default();
        match overlay.get(path) {
            Some(Some(overlay_stats)) => tally_file(acc, overlay_stats, true, true),
            Some(None) => {
                acc.base_files += 1;
                acc.overlay_entries += 1;
            }
            None => tally_file(acc, stats, true, false),
        }
    }

    // Process overlay entries not in base
    for (path, entry) in overlay {
        if base.contains_key(path) {
            continue;
        }
        let dir = dir_of(path);
        let acc = dirs.entry(dir.to_string()).or_default();
        if let Some(stats) = entry {
            tally_file(acc, stats, false, true);
        } else {
            acc.overlay_entries += 1;
        }
    }

    dirs.into_iter().collect()
}

impl JjRepoState {
    fn aggregate_stats(&self) -> DiffCounts {
        aggregate_overlay_stats(&self.base_file_stats, &self.overlay)
    }

    /// Build a RepoStatus with current aggregate diff stats.
    fn current_status(&self) -> RepoStatus {
        let c = self.aggregate_stats();
        RepoStatus {
            file_mad_count_working_tree: c.file_mad_count,
            lines_added_working_tree: c.lines_added,
            lines_removed_working_tree: c.lines_removed,
            files_modified_working_tree: c.files_modified,
            files_added_working_tree: c.files_added,
            files_deleted_working_tree: c.files_deleted,
            file_mad_count: c.file_mad_count,
            lines_added_total: c.lines_added,
            lines_removed_total: c.lines_removed,
            files_modified_total: c.files_modified,
            files_added_total: c.files_added,
            files_deleted_total: c.files_deleted,
            empty: c.file_mad_count == 0,
            ..self.base_status.clone()
        }
    }
}

/// Compute per-file diff stats between two trees, honoring rename/copy
/// records the way `jj diff --stat` does: a renamed file is one changed
/// file (its source's delete entry is suppressed by the copies stream),
/// with line stats from diffing source content against target content.
#[tracing::instrument(skip_all)]
async fn compute_per_file_diff_stats(
    store: &Arc<jj_lib::store::Store>,
    from_tree: &jj_lib::merged_tree::MergedTree,
    to_tree: &jj_lib::merged_tree::MergedTree,
    marker_style: ConflictMarkerStyle,
    copy_records: &jj_lib::copies::CopyRecords,
) -> HashMap<String, FileDiffStats> {
    let mut result = HashMap::new();

    // Materialize with *unlabeled* conflict markers on both sides, exactly
    // like jj's DiffStats::calculate — per-tree labels differ between
    // parent and child commits, and label text must never count as changed
    // lines.
    let unlabeled = jj_lib::conflict_labels::ConflictLabels::unlabeled();
    let unlabeled = &unlabeled;

    // Materialize both sides of each entry with buffered concurrency,
    // mirroring jj-lib's own materialized_diff_stream — serial awaits here
    // would bottleneck large diffs on backend read latency.
    let mut diff_stream = from_tree
        .diff_stream_with_copies(to_tree, &EverythingMatcher, copy_records)
        .map(|entry| async move {
            let values = entry.values.ok()?;
            let (before, after) = futures::join!(
                materialized_content(
                    store,
                    entry.path.source(),
                    values.before,
                    unlabeled,
                    marker_style,
                ),
                materialized_content(
                    store,
                    entry.path.target(),
                    values.after,
                    unlabeled,
                    marker_style,
                ),
            );
            Some((entry.path, before, after))
        })
        .buffered((store.concurrency() / 2).max(1));
    while let Some(entry) = diff_stream.next().await {
        let Some((path, before, after)) = entry else {
            continue;
        };

        let renamed_from = path
            .copy_operation()
            .map(|_| path.source().as_internal_file_string().to_string());

        let stats = match diff_stats_for_contents(before.as_deref(), after.as_deref()) {
            Some(mut stats) => {
                if renamed_from.is_some() {
                    // A rename/copy target always counts as one changed file,
                    // classified as modified, even when content is identical.
                    stats.kind = FileChangeKind::Modified;
                    stats.content_hash = None;
                    stats.renamed_from = renamed_from;
                }
                stats
            }
            None if renamed_from.is_some() => FileDiffStats {
                kind: FileChangeKind::Modified,
                renamed_from,
                ..Default::default()
            },
            // The stream yields entries only when tree values differ, so
            // equal materialized contents (conflict-shape change resolving
            // to the same text, exec-bit flip) still count as one changed
            // file with zero lines — jj shows `file | 0`.
            None if before.is_some() => FileDiffStats {
                kind: FileChangeKind::Modified,
                ..Default::default()
            },
            None => continue,
        };

        result.insert(path.target().as_internal_file_string().to_string(), stats);
    }

    result
}

/// Gather rename/copy records between each parent and the given commit,
/// as jj's diff machinery does. Failures degrade to "no records".
async fn gather_copy_records(
    store: &Arc<jj_lib::store::Store>,
    commit: &jj_lib::commit::Commit,
) -> jj_lib::copies::CopyRecords {
    let mut records = jj_lib::copies::CopyRecords::default();
    for parent_id in commit.parent_ids() {
        let Ok(stream) = store.get_copy_records(None, parent_id, commit.id()) else {
            continue;
        };
        let collected: Vec<_> = stream.collect().await;
        if let Err(e) = records.add_records(collected) {
            tracing::debug!(error = %e, "failed to add copy records");
        }
    }
    records
}

/// Compute `diff --stat`-style stats for a pair of materialized contents.
///
/// Returns `None` when both sides are absent or byte-identical (no change).
/// Binary content counts the file with 0 lines, matching `diff --stat`.
fn diff_stats_for_contents(before: Option<&[u8]>, after: Option<&[u8]>) -> Option<FileDiffStats> {
    let mut stats = FileDiffStats::default();
    match (before, after) {
        (None, None) => return None,
        (None, Some(content)) => {
            stats.kind = FileChangeKind::Added;
            stats.content_hash = content_fingerprint(content);
            if !is_binary(content) {
                stats.lines_added = count_lines(content);
            }
        }
        (Some(content), None) => {
            stats.kind = FileChangeKind::Deleted;
            stats.content_hash = content_fingerprint(content);
            if !is_binary(content) {
                stats.lines_removed = count_lines(content);
            }
        }
        (Some(before), Some(after)) => {
            if before == after {
                return None;
            }
            stats.kind = FileChangeKind::Modified;
            if !is_binary(before) && !is_binary(after) {
                let diff = diff_by_line([before, after], &LineCompareMode::Exact);
                for hunk in diff.hunks() {
                    if hunk.kind == DiffHunkKind::Different {
                        stats.lines_removed += count_lines(hunk.contents[0].as_ref());
                        stats.lines_added += count_lines(hunk.contents[1].as_ref());
                    }
                }
            }
        }
    }
    Some(stats)
}

/// Compute aggregate diff stats from a per-file map.
fn aggregate_file_stats(per_file: &HashMap<String, FileDiffStats>) -> DiffCounts {
    let empty = HashMap::new();
    aggregate_overlay_stats(per_file, &empty)
}

/// Diff a single file on disk against its parent tree version.
///
/// Returns `Some(stats)` if the file differs from the parent, `None` if identical
/// or both sides are absent.
async fn diff_single_file(
    store: &Arc<jj_lib::store::Store>,
    parent_tree: &jj_lib::merged_tree::MergedTree,
    repo_path: &jj_lib::repo_path::RepoPath,
    disk_content: Option<&[u8]>,
    marker_style: ConflictMarkerStyle,
) -> Option<FileDiffStats> {
    use jj_lib::conflict_labels::ConflictLabels;
    use jj_lib::conflicts::{
        ConflictMaterializeOptions, MaterializedTreeValue, choose_materialized_conflict_marker_len,
        materialize_merge_result_to_bytes, materialize_tree_value, parse_conflict,
    };

    let parent_value = parent_tree.path_value(repo_path).ok()?;
    let materialized = materialize_tree_value(store, repo_path, parent_value, parent_tree.labels())
        .await
        .ok()?;

    if let MaterializedTreeValue::FileConflict(file) = materialized {
        // jj's `diff --stat` never diffs raw disk text against a conflicted
        // parent: the snapshot first parses conflict markers back into a
        // structured conflict (falling back to resolved bytes when parsing
        // fails), and stats then materialize BOTH sides with *unlabeled*
        // markers so label text never counts as changed lines (see jj's
        // DiffStats::calculate). Mirror that pipeline here.
        let marker_len = choose_materialized_conflict_marker_len(&file.contents);
        let options = ConflictMaterializeOptions {
            marker_style,
            marker_len: Some(marker_len),
            merge: store.merge_options().clone(),
        };
        let unlabeled = ConflictLabels::unlabeled();
        let before: Vec<u8> =
            materialize_merge_result_to_bytes(&file.contents, &unlabeled, &options).into();
        let after: Option<Vec<u8>> = disk_content.map(|disk| {
            match parse_conflict(disk, file.contents.num_sides(), marker_len) {
                Some(hunks) => {
                    // Reassemble per-side contents from the parsed hunks (as
                    // jj's snapshot does) and rematerialize with the same
                    // unlabeled markers, so an untouched or side-edited
                    // conflict diffs by content, not by marker/label text.
                    let mut sides: jj_lib::merge::Merge<Vec<u8>> =
                        file.contents.map(|_| Vec::new());
                    for hunk in hunks {
                        if let Some(resolved) = hunk.as_resolved() {
                            for side in sides.iter_mut() {
                                side.extend_from_slice(resolved);
                            }
                        } else {
                            for (side, piece) in std::iter::zip(sides.iter_mut(), hunk.iter()) {
                                side.extend_from_slice(piece);
                            }
                        }
                    }
                    materialize_merge_result_to_bytes(&sides, &unlabeled, &options).into()
                }
                // No parseable markers: jj snapshots the raw bytes as
                // resolved content.
                None => disk.to_vec(),
            }
        });
        return diff_stats_for_contents(Some(&before), after.as_deref());
    }

    let parent_content = materialized_bytes(store, repo_path, materialized, marker_style).await;
    diff_stats_for_contents(parent_content.as_deref(), disk_content)
}

/// A file's value at the snapshot level — the shape jj's working-copy
/// snapshot records, abstracted from store ids to contents (content
/// equality implies id equality, so structural equality of `WcValue`s
/// matches jj's tree-value equality for everything the daemon tracks).
#[derive(Clone, PartialEq, Eq)]
enum WcValue {
    Absent,
    /// Resolved content bytes (plain file, symlink target as text) plus the
    /// executable bit: jj's snapshot records the disk exec bit in the tree
    /// value, so an exec-only flip is a value change (`jj diff --stat` shows
    /// `file | 0`). Symlinks and other non-file values use `false`.
    Resolved {
        content: Vec<u8>,
        executable: bool,
    },
    /// Unresolved conflict: simplified per-side contents. Exec bits are NOT
    /// modeled here: while a file stays conflicted, jj's snapshot returns the
    /// current tree values unchanged for exec-only disk flips
    /// (`write_path_to_store` keeps `current_tree_values` when the parsed
    /// file ids are unchanged), so the flip is invisible to `jj diff`.
    Conflict(jj_lib::merge::Merge<Vec<u8>>),
}

/// A [`WcValue`] plus, when it mirrors an existing tree value, the stored
/// value itself. jj's diff emits an entry whenever the *stored* values
/// differ — two conflicts with equal simplified contents but different
/// shapes still show as `file | 0` — so equality compares raw values when
/// both sides are tree-backed, contents otherwise (a value derived from
/// disk gets ids from its content at snapshot time).
struct WcEntry {
    raw: Option<jj_lib::merge::MergedTreeValue>,
    value: WcValue,
}

impl WcEntry {
    fn derived(value: WcValue) -> Self {
        WcEntry { raw: None, value }
    }

    fn same_value_as(&self, other: &WcEntry) -> bool {
        match (&self.raw, &other.raw) {
            (Some(a), Some(b)) => a == b,
            _ => self.value == other.value,
        }
    }
}

/// Normalize conflict sides: simplification may resolve the merge.
/// `resolved_exec` is the executable bit to use if the merge resolves
/// (jj takes it from the disk file when a conflict resolves at snapshot,
/// from the merged tree values when reading stored values).
fn normalize_conflict_sides(sides: jj_lib::merge::Merge<Vec<u8>>, resolved_exec: bool) -> WcValue {
    let simplified = sides.simplify();
    match simplified.as_resolved() {
        Some(content) => WcValue::Resolved {
            content: content.clone(),
            executable: resolved_exec,
        },
        None => WcValue::Conflict(simplified),
    }
}

/// Read a tree's value at `path` as a [`WcEntry`].
async fn tree_wc_value(
    store: &Arc<jj_lib::store::Store>,
    tree: &jj_lib::merged_tree::MergedTree,
    path: &jj_lib::repo_path::RepoPath,
) -> WcEntry {
    let Ok(raw) = tree.path_value(path) else {
        return WcEntry::derived(WcValue::Absent);
    };
    raw_wc_value(store, path, raw).await
}

/// Build a [`WcEntry`] from a raw tree value.
async fn raw_wc_value(
    store: &Arc<jj_lib::store::Store>,
    path: &jj_lib::repo_path::RepoPath,
    raw: jj_lib::merge::MergedTreeValue,
) -> WcEntry {
    use jj_lib::conflicts::{MaterializedTreeValue, materialize_tree_value};

    let unlabeled = jj_lib::conflict_labels::ConflictLabels::unlabeled();
    let Ok(materialized) = materialize_tree_value(store, path, raw.clone(), &unlabeled).await
    else {
        return WcEntry::derived(WcValue::Absent);
    };
    let value = match materialized {
        MaterializedTreeValue::Absent
        | MaterializedTreeValue::AccessDenied(_)
        | MaterializedTreeValue::GitSubmodule(_)
        | MaterializedTreeValue::Tree(_) => WcValue::Absent,
        MaterializedTreeValue::File(mut file) => match file.read_all(path).await {
            Ok(content) => WcValue::Resolved {
                content,
                executable: file.executable,
            },
            Err(_) => WcValue::Absent,
        },
        MaterializedTreeValue::Symlink { target, .. } => WcValue::Resolved {
            content: target.into_bytes(),
            executable: false,
        },
        MaterializedTreeValue::FileConflict(file) => {
            let resolved_exec = file.executable.unwrap_or(false);
            normalize_conflict_sides(file.contents.map(|side| side.to_vec()), resolved_exec)
        }
        MaterializedTreeValue::OtherConflict { id, labels } => WcValue::Resolved {
            content: id.describe(&labels).into_bytes(),
            executable: false,
        },
    };
    WcEntry {
        raw: Some(raw),
        value,
    }
}

/// Synthesize the value jj's next snapshot would record for `path`, given
/// the current disk content — the read-only mirror of jj-lib's
/// `update_from_content` decision tree (conflicts.rs), built from the same
/// primitives: `files::merge_hunks` classifies the existing conflict's
/// hunks, `conflicts::parse_conflict` parses markers back from disk (with
/// the WC value's simplified arity and the checkout's marker length via
/// `choose_materialized_conflict_marker_len`), and the "unchanged" checks
/// retain the WC value verbatim. (`update_from_content` itself writes
/// blobs to the store, which the daemon must never do.)
fn synthetic_wc_value(
    store: &Arc<jj_lib::store::Store>,
    commit_entry: WcEntry,
    disk_content: Option<&[u8]>,
    disk_exec: bool,
    visible_if_untracked: bool,
) -> WcEntry {
    use jj_lib::conflicts::{choose_materialized_conflict_marker_len, parse_conflict};
    use jj_lib::files::MergeResult;

    let Some(disk) = disk_content else {
        return WcEntry::derived(WcValue::Absent);
    };
    let resolved_from_disk = || {
        WcEntry::derived(WcValue::Resolved {
            content: disk.to_vec(),
            executable: disk_exec,
        })
    };
    match &commit_entry.value {
        // Untracked: the snapshot starts tracking the file unless it is
        // ignored — an ignored untracked file is invisible to jj.
        WcValue::Absent => {
            if visible_if_untracked {
                resolved_from_disk()
            } else {
                commit_entry
            }
        }
        // Tracked resolved file: the snapshot stores the disk content and
        // the disk exec bit (`ExecBit::new_from_disk` → `for_tree_value`).
        WcValue::Resolved { .. } => resolved_from_disk(),
        // Tracked conflict: parse markers back, `update_from_content`-style.
        WcValue::Conflict(sides) => {
            let old_hunks = jj_lib::files::merge_hunks(sides, store.merge_options());
            let marker_len = choose_materialized_conflict_marker_len(sides);
            let new_hunks = parse_conflict(disk, sides.num_sides(), marker_len);
            // "Unchanged" short-circuit: keep the WC value so unchanged
            // conflicts aren't updated to partially-resolved contents.
            let unchanged = match (&old_hunks, &new_hunks) {
                (MergeResult::Resolved(old), None) => &old[..] == disk,
                (MergeResult::Conflict(old), Some(new)) => old == new,
                _ => false,
            };
            if unchanged {
                return commit_entry;
            }
            match new_hunks {
                // No parseable markers: resolved to the raw bytes (a conflict
                // resolving at snapshot takes the exec bit from disk).
                None => resolved_from_disk(),
                Some(hunks) => {
                    let mut new_sides: jj_lib::merge::Merge<Vec<u8>> = sides.map(|_| Vec::new());
                    for hunk in hunks {
                        if let Some(resolved) = hunk.as_resolved() {
                            for side in new_sides.iter_mut() {
                                side.extend_from_slice(resolved);
                            }
                        } else {
                            for (side, piece) in std::iter::zip(new_sides.iter_mut(), hunk.iter()) {
                                side.extend_from_slice(piece);
                            }
                        }
                    }
                    WcEntry::derived(normalize_conflict_sides(new_sides, disk_exec))
                }
            }
        }
    }
}

/// Materialize a [`WcValue`] to the unlabeled text jj's `diff --stat`
/// compares (conflicts render with unlabeled markers, DiffStats-style).
fn wc_value_text(
    store: &Arc<jj_lib::store::Store>,
    value: &WcValue,
    marker_style: ConflictMarkerStyle,
) -> Option<Vec<u8>> {
    use jj_lib::conflicts::{
        ConflictMaterializeOptions, choose_materialized_conflict_marker_len,
        materialize_merge_result_to_bytes,
    };

    match value {
        WcValue::Absent => None,
        WcValue::Resolved { content, .. } => Some(content.clone()),
        WcValue::Conflict(sides) => {
            let options = ConflictMaterializeOptions {
                marker_style,
                marker_len: Some(choose_materialized_conflict_marker_len(sides)),
                merge: store.merge_options().clone(),
            };
            let unlabeled = jj_lib::conflict_labels::ConflictLabels::unlabeled();
            Some(materialize_merge_result_to_bytes(sides, &unlabeled, &options).into())
        }
    }
}

/// Diff two snapshot-level values jj-style: an entry exists iff the values
/// differ; line stats come from the unlabeled texts, so a value change with
/// identical text (conflict-shape change, hunk-resolvable conflict vs its
/// resolution) yields a zero-line Modified entry (`file | 0`).
fn diff_wc_values(
    store: &Arc<jj_lib::store::Store>,
    before: &WcEntry,
    after: &WcEntry,
    marker_style: ConflictMarkerStyle,
) -> Option<FileDiffStats> {
    if before.same_value_as(after) {
        return None;
    }
    let before_text = wc_value_text(store, &before.value, marker_style);
    let after_text = wc_value_text(store, &after.value, marker_style);
    match diff_stats_for_contents(before_text.as_deref(), after_text.as_deref()) {
        Some(stats) => Some(stats),
        None => Some(FileDiffStats {
            kind: FileChangeKind::Modified,
            ..Default::default()
        }),
    }
}

/// `Merge::get_simplified_mapping`'s algorithm with a caller-supplied
/// equality predicate over ORIGINAL term indices: cancel (remove, add)
/// pairs whose terms compare equal, returning the surviving original
/// indices (interleaved add/remove order preserved).
fn simplified_mapping_by(len: usize, eq: &dyn Fn(usize, usize) -> bool) -> Vec<usize> {
    let mut indices: Vec<usize> = (0..len).collect();
    let mut add_index = 0;
    while add_index < indices.len() {
        let add_original = indices[add_index];
        let found = indices
            .iter()
            .enumerate()
            .skip(1)
            .step_by(2)
            .find(|(_, original_remove)| eq(**original_remove, add_original))
            .map(|(remove_index, _)| remove_index);
        if let Some(remove_index) = found {
            indices.swap(remove_index + 1, add_index);
            indices.drain(remove_index..remove_index + 2);
        } else {
            add_index += 2;
        }
    }
    indices
}

/// Whether two root-tree terms become equal after this batch's overrides:
/// either the tree ids already match, or every differing path is one the
/// batch overwrote with a RESOLVED value (a resolved override is applied
/// identically to every term, erasing the difference). Walks only the
/// subtrees where the two terms differ.
async fn terms_equal_after_overrides(
    store: &Arc<jj_lib::store::Store>,
    a: &jj_lib::backend::TreeId,
    b: &jj_lib::backend::TreeId,
    collapsed_paths: &HashSet<String>,
) -> bool {
    use futures::StreamExt as _;

    if a == b {
        return true;
    }
    if collapsed_paths.is_empty() {
        return false;
    }
    let unlabeled = jj_lib::conflict_labels::ConflictLabels::unlabeled();
    let tree_a = jj_lib::merged_tree::MergedTree::new(
        store.clone(),
        jj_lib::merge::Merge::resolved(a.clone()),
        unlabeled.clone(),
    );
    let tree_b = jj_lib::merged_tree::MergedTree::new(
        store.clone(),
        jj_lib::merge::Merge::resolved(b.clone()),
        unlabeled,
    );
    let mut stream = tree_a.diff_stream(&tree_b, &EverythingMatcher);
    while let Some(entry) = stream.next().await {
        if !collapsed_paths.contains(entry.path.as_internal_file_string()) {
            return false;
        }
    }
    true
}

/// Count parents that would produce a gix copy/rename record for
/// (source → target-with-this-content): the source present in that
/// parent's gix-visible tree, the target absent there, and the source's
/// content >= 50% similar to the target's. gix re-detects on every diff,
/// so a recorded pairing is only as durable as the current similarity.
async fn rename_record_parents(
    state: &JjRepoState,
    source_rel: &str,
    target_rel: &str,
    target_disk: Option<&[u8]>,
) -> usize {
    let Some(disk) = target_disk else {
        return 0;
    };
    if disk.is_empty() {
        return 0;
    }
    let Ok(source_path) = jj_lib::repo_path::RepoPathBuf::from_relative_path(source_rel) else {
        return 0;
    };
    let Ok(target_path) = jj_lib::repo_path::RepoPathBuf::from_relative_path(target_rel) else {
        return 0;
    };
    let parent_trees: Vec<&jj_lib::merged_tree::MergedTree> = if state.parent_trees.is_empty() {
        vec![&state.parent_tree]
    } else {
        state.parent_trees.iter().collect()
    };
    let mut records = 0;
    for tree in parent_trees {
        let Ok(value) = tree.path_value(&source_path) else {
            continue;
        };
        if value.is_absent() {
            continue;
        }
        let target_absent = tree
            .path_value(&target_path)
            .is_ok_and(|value| value.is_absent());
        if !target_absent {
            continue;
        }
        let Some(content) = materialized_content(
            &state.store,
            &source_path,
            value,
            tree.labels(),
            state.conflict_marker_style,
        )
        .await
        else {
            continue;
        };
        if content.is_empty() {
            continue;
        }
        if content_similarity(&content, disk) >= 0.5 {
            records += 1;
        }
    }
    records
}

/// Whether a rename/copy source path re-created on disk is *visible* to jj.
/// A file that is untracked (absent from the WC commit tree) AND ignored is
/// invisible to the snapshot, so its presence on disk does not void a
/// rename premise — jj keeps pairing `{dir => dir2}/f` while the re-created
/// source is ignored.
fn source_visible_on_disk(state: &JjRepoState, source_rel: &str) -> bool {
    let abs = state.repo_root.join(source_rel);
    if !abs.exists() {
        return false;
    }
    let tracked = jj_lib::repo_path::RepoPathBuf::from_relative_path(source_rel)
        .ok()
        .and_then(|path| state.commit_tree.path_value(&path).ok())
        .is_some_and(|value| !value.is_absent());
    if tracked {
        return true;
    }
    let vcs_dir = state.repo_root.join(".jj");
    let git_dir = state.repo_root.join(".git");
    let colocated_git_dir = git_dir.exists().then_some(git_dir);
    let verdict = state.ignore_filter.process_event(
        &vcs_dir,
        colocated_git_dir.as_deref(),
        std::slice::from_ref(&abs),
    );
    !verdict.changed_paths.is_empty()
}

/// Diff each changed path on disk against the parent tree and record the
/// results in the overlay. Paths are deduplicated — each is re-read from
/// disk at processing time, so duplicates are pure wasted work.
async fn apply_incremental_paths(state: &mut JjRepoState, changed_paths: &[PathBuf]) {
    // ui.conflict-marker-style can change between snapshots without any op
    // or watcher event, and jj's next `diff --stat` uses the current value —
    // re-resolve per batch (cheap: config reads are mtime-cached).
    state.conflict_marker_style = conflict_marker_style_for_repo(&state.repo_root);

    // Classify paths through the ignore rules (this also lazily ingests any
    // .gitignore/.jjignore files present in the batch).
    let vcs_dir = state.repo_root.join(".jj");
    let colocated_git_dir = {
        let git_dir = state.repo_root.join(".git");
        git_dir.exists().then_some(git_dir)
    };
    let verdict =
        state
            .ignore_filter
            .process_event(&vcs_dir, colocated_git_dir.as_deref(), changed_paths);
    let not_ignored: HashSet<&Path> = verdict.changed_paths.iter().map(|p| p.as_path()).collect();

    // Per-path results are staged, then a rename post-pass pairs same-batch
    // delete+add before committing anything to the overlay.
    let mut staged: Vec<(String, Option<FileDiffStats>)> = Vec::new();
    let mut seen = HashSet::new();
    // Does this batch make the next snapshot actually write a new tree?
    let mut snapshot_writes = false;
    // Paths whose staged value was derived from disk this batch (must not
    // be overridden by the conflict-simplification post-pass).
    let mut derived_this_batch: HashSet<String> = HashSet::new();
    // Batch paths whose new value is RESOLVED (or absent): the snapshot's
    // tree builder applies these identically to every root-tree term,
    // erasing inter-term differences at that path.
    let mut resolved_override_paths: HashSet<String> = HashSet::new();
    for abs_path in changed_paths {
        if !seen.insert(abs_path) {
            continue;
        }
        let Some(rel_str) = abs_to_repo_relative(&state.repo_root, abs_path) else {
            continue;
        };
        // Read file from disk (None if deleted/missing), plus the exec bit
        // jj's snapshot would record (`mode & 0o111`, like ExecBit::new_from_disk).
        let disk_content = std::fs::read(abs_path).ok();
        let disk_exec = {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                std::fs::metadata(abs_path)
                    .map(|m| m.permissions().mode() & 0o111 != 0)
                    .unwrap_or(false)
            }
            #[cfg(not(unix))]
            {
                false
            }
        };

        // If the base recorded this path as a rename target, diff the disk
        // content against the *source's* parent content, as jj's copy-aware
        // diff does — otherwise the target looks like a fresh Add. The
        // premise must be re-validated against the CURRENT state: gix
        // re-detects on every diff, so the pairing holds only while
        // - the source stays invisible to jj (a visibly re-created source
        //   turns the target back into a plain file; an ignored untracked
        //   re-creation does not), and
        // - exactly one parent still produces a record for the target's
        //   new content. Zero records (target rewritten dissimilarly) void
        //   the pairing AND resurface the source's suppressed delete;
        //   two or more hit jj-lib's duplicate-record poisoning (plain-add
        //   target, delete stays dropped).
        let base_renamed_from = state
            .base_file_stats
            .get(&rel_str)
            .and_then(|s| s.renamed_from.clone());
        let mut resurrect_source: Option<String> = None;
        let renamed_from = match base_renamed_from {
            Some(src) if !source_visible_on_disk(state, &src) => {
                match rename_record_parents(state, &src, &rel_str, disk_content.as_deref()).await {
                    1 => Some(src),
                    0 => {
                        resurrect_source = Some(src);
                        None
                    }
                    _ => None,
                }
            }
            _ => None,
        };
        let had_renamed_from = renamed_from.is_some();
        let parent_lookup = renamed_from.as_deref().unwrap_or(&rel_str);
        let Ok(repo_path_buf) = jj_lib::repo_path::RepoPathBuf::from_relative_path(parent_lookup)
        else {
            continue;
        };
        if let Some(src_rel) = resurrect_source
            && let Ok(src_path) = jj_lib::repo_path::RepoPathBuf::from_relative_path(&src_rel)
        {
            // The base suppressed the source's delete on behalf of the
            // now-void pairing; recompute its plain entry. Disk content is
            // passed as None deliberately: the source is invisible to the
            // snapshot (absent, or ignored and untracked).
            let parent_value = tree_wc_value(&state.store, &state.parent_tree, &src_path).await;
            let commit_entry = tree_wc_value(&state.store, &state.commit_tree, &src_path).await;
            let synthetic = synthetic_wc_value(&state.store, commit_entry, None, false, false);
            let plain = diff_wc_values(
                &state.store,
                &parent_value,
                &synthetic,
                state.conflict_marker_style,
            );
            staged.push((src_rel, plain));
        }

        // Whether jj's next snapshot would WRITE for this path (its stored
        // value changes). Any write triggers the tree-wide conflict
        // simplification mirrored after this loop.
        let target_path_buf = jj_lib::repo_path::RepoPathBuf::from_relative_path(&rel_str).ok();
        if let Some(target_path) = &target_path_buf {
            let commit_entry = tree_wc_value(&state.store, &state.commit_tree, target_path).await;
            let visible_if_untracked = had_renamed_from || not_ignored.contains(abs_path.as_path());
            let synthetic = synthetic_wc_value(
                &state.store,
                WcEntry {
                    raw: commit_entry.raw.clone(),
                    value: commit_entry.value.clone(),
                },
                disk_content.as_deref(),
                disk_exec,
                visible_if_untracked,
            );
            if !synthetic.same_value_as(&commit_entry) {
                snapshot_writes = true;
                if matches!(synthetic.value, WcValue::Resolved { .. } | WcValue::Absent) {
                    resolved_override_paths.insert(rel_str.clone());
                }
            }
            if synthetic.raw.is_none() {
                derived_this_batch.insert(rel_str.clone());
            }
        }

        let mut diff_result = if had_renamed_from {
            // Copy-record redirect: diff disk content against the SOURCE's
            // parent value (jj's resolve_copy_source), and keep the target
            // classified as a rename/copy target.
            derived_this_batch.insert(rel_str.clone());
            let mut result = diff_single_file(
                &state.store,
                &state.parent_tree,
                &repo_path_buf,
                disk_content.as_deref(),
                state.conflict_marker_style,
            )
            .await;
            match &mut result {
                // A rename target that now matches the source content is
                // still one changed file (the rename itself), not "no
                // change".
                None => {
                    result = Some(FileDiffStats {
                        kind: FileChangeKind::Modified,
                        renamed_from,
                        ..Default::default()
                    });
                }
                Some(stats) if stats.kind == FileChangeKind::Modified => {
                    stats.renamed_from = renamed_from;
                    stats.content_hash = None;
                }
                _ => {}
            }
            result
        } else {
            // Snapshot-faithful path: synthesize the value jj's next
            // snapshot would record for this path, and diff it against the
            // parent value. An entry exists iff the values differ, which
            // covers trackedness (a file snapshotted before a .gitignore
            // covered it stays tracked), ignored-untracked invisibility,
            // and standing zero-line value changes (`file | 0`) uniformly.
            let visible_if_untracked = not_ignored.contains(abs_path.as_path());
            let parent_value =
                tree_wc_value(&state.store, &state.parent_tree, &repo_path_buf).await;
            let commit_entry =
                tree_wc_value(&state.store, &state.commit_tree, &repo_path_buf).await;
            let synthetic = synthetic_wc_value(
                &state.store,
                commit_entry,
                disk_content.as_deref(),
                disk_exec,
                visible_if_untracked,
            );
            diff_wc_values(
                &state.store,
                &parent_value,
                &synthetic,
                state.conflict_marker_style,
            )
        };

        // Events on a recorded rename *source* (some target maps back to
        // this path via renamed_from, in the base or the overlay):
        // - source still invisible to jj (gone, or re-created but ignored
        //   and untracked) → its entry is subsumed by the rename (jj's
        //   copies stream suppresses the source's delete);
        // - source visibly re-created → the rename premise is void:
        //   re-diff every target as a plain file (jj shows a plain add —
        //   a rename needs the source to be gone).
        let rename_targets: Vec<String> = state
            .base_file_stats
            .iter()
            .filter(|(_, s)| s.renamed_from.as_deref() == Some(rel_str.as_str()))
            .map(|(target, _)| target.clone())
            .chain(
                state
                    .overlay
                    .iter()
                    .filter(|(_, entry)| {
                        entry
                            .as_ref()
                            .is_some_and(|s| s.renamed_from.as_deref() == Some(rel_str.as_str()))
                    })
                    .map(|(target, _)| target.clone()),
            )
            .collect();
        if !rename_targets.is_empty() {
            if !source_visible_on_disk(state, &rel_str) {
                diff_result = None;
            } else {
                for target_rel in rename_targets {
                    let Ok(target_path) =
                        jj_lib::repo_path::RepoPathBuf::from_relative_path(&target_rel)
                    else {
                        continue;
                    };
                    let target_disk = std::fs::read(state.repo_root.join(&target_rel)).ok();
                    let plain = diff_single_file(
                        &state.store,
                        &state.parent_tree,
                        &target_path,
                        target_disk.as_deref(),
                        state.conflict_marker_style,
                    )
                    .await;
                    staged.push((target_rel, plain));
                }
            }
        }

        staged.push((rel_str, diff_result));
    }

    // Same-batch rename pairing, mirroring jj's copy detection. jj computes
    // copy records at diff time via gix rename tracking between the *diff
    // endpoints* — source content in the parent tree vs target content in
    // the working copy — pairing at >= 50% similarity
    // (`GitBackend::get_copy_records`: `gix::diff::Rewrites { percentage:
    // Some(0.5), .. }`). A pair becomes one rename target diffed against
    // the source's parent content (jj's `{a => b} | N` fuzzy stat) and the
    // source's delete entry is suppressed, as jj's copies stream does.
    //
    // A snapshot that actually writes re-simplifies the root tree merge
    // (`MergedTreeBuilder::write_tree`: overrides per term, then root-level
    // `simplify_with` + `resolve()`). A (remove, add) TERM pair cancels
    // when the two whole trees become equal after the batch's overrides —
    // a resolved override is applied identically to every term, so term
    // pairs whose differences are confined to resolved-override paths
    // cancel, dropping those terms from EVERY path's stored value.
    // Untouched conflicted paths then differ from the parent by shape with
    // identical contents: jj reports `file | 0`. (A conflicted override
    // keeps terms distinct — no cancellation, no phantom entries.)
    // Bounded by the number of conflicts, never O(repo).
    if snapshot_writes && !state.conflicted_paths.is_empty() {
        let term_ids: Vec<jj_lib::backend::TreeId> =
            state.commit_tree.tree_ids().iter().cloned().collect();
        let len = term_ids.len();
        let mut eq = vec![vec![false; len]; len];
        for i in 0..len {
            eq[i][i] = true;
            for j in (i + 1)..len {
                let equal = terms_equal_after_overrides(
                    &state.store,
                    &term_ids[i],
                    &term_ids[j],
                    &resolved_override_paths,
                )
                .await;
                eq[i][j] = equal;
                eq[j][i] = equal;
            }
        }
        let mapping = simplified_mapping_by(len, &|a, b| eq[a][b]);
        if mapping.len() != len {
            let same_change = state.store.merge_options().same_change;
            for path in &state.conflicted_paths {
                let rel = path.as_internal_file_string().to_string();
                if derived_this_batch.contains(&rel) {
                    continue;
                }
                let Ok(stored) = state.commit_tree.path_value(path) else {
                    continue;
                };
                let values: Vec<_> = stored.iter().cloned().collect();
                if values.len() != len {
                    continue;
                }
                let projected = jj_lib::merge::Merge::from_vec(
                    mapping
                        .iter()
                        .map(|&i| values[i].clone())
                        .collect::<Vec<_>>(),
                );
                // jj's subsequent resolve() preserves arity except for
                // trivial per-path resolution.
                let projected = match projected.resolve_trivial(same_change) {
                    Some(value) => jj_lib::merge::Merge::resolved(value.clone()),
                    None => projected,
                };
                let new_entry = raw_wc_value(&state.store, path, projected).await;
                let parent_entry = tree_wc_value(&state.store, &state.parent_tree, path).await;
                let entry = diff_wc_values(
                    &state.store,
                    &parent_entry,
                    &new_entry,
                    state.conflict_marker_style,
                );
                staged.push((rel, entry));
            }
        }
    }

    // Pairing operates on the *composite* diff state (staged over overlay
    // over base), not just this batch: jj's copy detection at the next
    // snapshot sees the whole diff, so a delete recorded batches ago pairs
    // with an add arriving now.
    let effective = |path: &str, staged: &[(String, Option<FileDiffStats>)]| {
        if let Some((_, result)) = staged.iter().rev().find(|(p, _)| p == path) {
            return result.clone();
        }
        if let Some(entry) = state.overlay.get(path) {
            return entry.clone();
        }
        state.base_file_stats.get(path).cloned()
    };
    let all_paths: HashSet<String> = state
        .base_file_stats
        .keys()
        .chain(state.overlay.keys())
        .cloned()
        .chain(staged.iter().map(|(path, _)| path.clone()))
        .collect();
    let parent_trees: Vec<&jj_lib::merged_tree::MergedTree> = if state.parent_trees.is_empty() {
        vec![&state.parent_tree]
    } else {
        state.parent_trees.iter().collect()
    };

    // Candidate rename targets: effective Adds; under a merge parent,
    // effective Modifies qualify too — gix pairs additions per parent, so a
    // path that arrived from the *other* merge side (present in the merged
    // parent tree) is still an addition relative to the source-owning
    // parent, and the old target baseline vanishes (`{e.rs => d.txt} | 0`).
    let mut candidates: Vec<(String, Vec<u8>)> = Vec::new();
    let mut sources: Vec<String> = Vec::new();
    for path in &all_paths {
        let Some(stats) = effective(path, &staged) else {
            continue;
        };
        if stats.renamed_from.is_some() {
            continue;
        }
        match stats.kind {
            FileChangeKind::Deleted => sources.push(path.clone()),
            FileChangeKind::Added => {}
            FileChangeKind::Modified if state.parent_trees.len() > 1 => {}
            _ => continue,
        }
        if stats.kind != FileChangeKind::Deleted {
            let Ok(disk) = std::fs::read(state.repo_root.join(path)) else {
                continue;
            };
            if disk.is_empty() {
                continue;
            }
            candidates.push((path.clone(), disk));
        }
    }
    sources.sort();
    candidates.sort_by(|(a, _), (b, _)| a.cmp(b));
    if !candidates.is_empty() {
        // gix runs copy detection per parent: a record for (source, target)
        // arises from parent P when the source is present in P, the target
        // is absent in P, and source@P is >= 50% similar to target@wc.
        // Crucially, the compared content is each parent's own gix-visible
        // blob — NOT the merged-parent materialization: a conflicted
        // source's marker text matches no parent-side blob, so jj pairs
        // nothing and shows a plain delete + add. The number of
        // record-producing parents decides the outcome: one → rename
        // pairing; two or more → jj-lib's duplicate-record poisoning
        // (`CopyRecords::add_records` discards duplicates: the target loses
        // its origin and becomes a plain add, while the source's delete
        // entry is still skipped by the stream's `has_source` check,
        // silently vanishing).
        for source_rel in sources {
            let Ok(source_path) = jj_lib::repo_path::RepoPathBuf::from_relative_path(&source_rel)
            else {
                continue;
            };

            // Materialize the source's content in each parent that has it.
            let mut parent_contents: Vec<(usize, Vec<u8>)> = Vec::new();
            for (pi, tree) in parent_trees.iter().enumerate() {
                let Ok(value) = tree.path_value(&source_path) else {
                    continue;
                };
                if value.is_absent() {
                    continue;
                }
                let Some(content) = materialized_content(
                    &state.store,
                    &source_path,
                    value,
                    tree.labels(),
                    state.conflict_marker_style,
                )
                .await
                else {
                    continue;
                };
                if content.is_empty() {
                    continue;
                }
                parent_contents.push((pi, content));
            }
            if parent_contents.is_empty() {
                continue;
            }

            // Best candidate by per-parent similarity, tracking how many
            // parents produce a record for it.
            let mut best: Option<(usize, f32, usize)> = None;
            for (ci, (target_rel, disk)) in candidates.iter().enumerate() {
                let Ok(target_path) =
                    jj_lib::repo_path::RepoPathBuf::from_relative_path(target_rel)
                else {
                    continue;
                };
                let mut records = 0usize;
                let mut score = 0.0f32;
                for (pi, content) in &parent_contents {
                    let target_absent = parent_trees[*pi]
                        .path_value(&target_path)
                        .is_ok_and(|value| value.is_absent());
                    if !target_absent {
                        continue;
                    }
                    let similarity = content_similarity(content, disk);
                    if similarity >= 0.5 {
                        records += 1;
                        score = score.max(similarity);
                    }
                }
                if records > 0 && best.is_none_or(|(_, s, _)| score > s) {
                    best = Some((ci, score, records));
                }
            }
            let Some((ci, _, records)) = best else {
                continue;
            };
            let (target_rel, target_disk) = candidates.swap_remove(ci);
            if records >= 2 {
                staged.push((source_rel, None));
                continue;
            }

            let mut stats = diff_single_file(
                &state.store,
                &state.parent_tree,
                &source_path,
                Some(&target_disk),
                state.conflict_marker_style,
            )
            .await
            // None (target matches the source's parent content) is still one
            // changed file: the rename itself.
            .unwrap_or_default();
            stats.kind = FileChangeKind::Modified;
            stats.content_hash = None;
            stats.renamed_from = Some(source_rel.clone());
            staged.push((target_rel, Some(stats)));
            staged.push((source_rel, None));
        }
    }

    // Copy detection, mirroring gix's `CopySource::FromSetOfModifiedFiles`
    // (jj's GitBackend::get_copy_records enables `copies` at 50%): an Added
    // entry >= 50% similar to a *modified* file becomes a COPY target —
    // `{src => dst} | N` with stats against the source's parent content —
    // while the source keeps its own modified entry. The similarity match
    // uses the source's NEW content (a gix Modification's `Change::id()` is
    // the post-image blob), while the resulting stats diff against the
    // source's parent value (resolve_copy_source). This can fire
    // retroactively: modifying a file converts an existing plain add into a
    // copy of it, and reverting the file dissolves the copy (handled by the
    // invalidation pass above plus this re-pairing). Duplicate records
    // (source qualifying via multiple parents) are discarded by jj-lib,
    // leaving the plain add.
    let mut modified_sources: Vec<String> = Vec::new();
    let mut added_targets: Vec<String> = Vec::new();
    for path in &all_paths {
        let Some(stats) = effective(path, &staged) else {
            continue;
        };
        if stats.renamed_from.is_some() {
            continue;
        }
        match stats.kind {
            FileChangeKind::Modified => modified_sources.push(path.clone()),
            FileChangeKind::Added => added_targets.push(path.clone()),
            _ => {}
        }
    }
    if !modified_sources.is_empty() && !added_targets.is_empty() {
        modified_sources.sort();
        added_targets.sort();
        for target_rel in &added_targets {
            let Ok(target_path) = jj_lib::repo_path::RepoPathBuf::from_relative_path(target_rel)
            else {
                continue;
            };
            let Ok(target_disk) = std::fs::read(state.repo_root.join(target_rel)) else {
                continue;
            };
            if target_disk.is_empty() {
                continue;
            }
            let mut best: Option<(&String, f32, usize)> = None;
            for source_rel in &modified_sources {
                let Ok(source_path) =
                    jj_lib::repo_path::RepoPathBuf::from_relative_path(source_rel)
                else {
                    continue;
                };
                // The copy-source content gix matches against is the
                // modified file's NEW (working-copy) content.
                let Ok(source_new) = std::fs::read(state.repo_root.join(source_rel)) else {
                    continue;
                };
                let similarity = content_similarity(&source_new, &target_disk);
                if similarity < 0.5 {
                    continue;
                }
                let records = parent_trees
                    .iter()
                    .filter(|tree| {
                        let source_present = tree
                            .path_value(&source_path)
                            .is_ok_and(|value| !value.is_absent());
                        let target_absent = tree
                            .path_value(&target_path)
                            .is_ok_and(|value| value.is_absent());
                        source_present && target_absent
                    })
                    .count();
                if records > 0 && best.is_none_or(|(_, s, _)| similarity > s) {
                    best = Some((source_rel, similarity, records));
                }
            }
            let Some((source_rel, _, records)) = best else {
                continue;
            };
            if records >= 2 {
                continue;
            }
            let Ok(source_path) = jj_lib::repo_path::RepoPathBuf::from_relative_path(source_rel)
            else {
                continue;
            };
            let mut stats = diff_single_file(
                &state.store,
                &state.parent_tree,
                &source_path,
                Some(&target_disk),
                state.conflict_marker_style,
            )
            .await
            .unwrap_or_default();
            stats.kind = FileChangeKind::Modified;
            stats.content_hash = None;
            stats.renamed_from = Some(source_rel.clone());
            staged.push((target_rel.clone(), Some(stats)));
        }
    }

    for (rel_str, diff_result) in staged {
        state.overlay.insert(rel_str, diff_result);
    }
}

/// Convert an absolute filesystem path to a repo-relative path string.
///
/// Tries direct strip_prefix first, then falls back to canonicalizing
/// the path (handling symlinks and macOS /var → /private/var).
/// For deleted files, canonicalizes the parent directory instead.
pub fn abs_to_repo_relative(repo_root: &Path, abs_path: &Path) -> Option<String> {
    // Fast path: direct prefix strip
    if let Ok(rel) = abs_path.strip_prefix(repo_root) {
        return Some(rel.to_string_lossy().replace('\\', "/"));
    }
    // Slow path: canonicalize (handles symlinks, /var → /private/var, etc.).
    // The path may be deleted — and so may its parent directories (e.g. a
    // checkout removed a file along with its now-empty directory) — so walk
    // up until an ancestor exists, canonicalize that, and re-append the rest.
    let canonical = abs_path.canonicalize().or_else(|err| {
        for ancestor in abs_path.ancestors().skip(1) {
            if let Ok(canonical_ancestor) = ancestor.canonicalize() {
                let rest = abs_path
                    .strip_prefix(ancestor)
                    .expect("ancestors() yields prefixes of abs_path");
                return Ok(canonical_ancestor.join(rest));
            }
        }
        Err(err)
    });
    if let Ok(canonical) = canonical {
        let rel = canonical.strip_prefix(repo_root).ok()?;
        return Some(rel.to_string_lossy().replace('\\', "/"));
    }
    None
}

/// Default revset alias definitions from jj-cli's config/revsets.toml.
const DEFAULT_TRUNK_ALIAS: &str = r#"latest(
    remote_bookmarks(exact:"main", exact:"origin") |
    remote_bookmarks(exact:"master", exact:"origin") |
    remote_bookmarks(exact:"trunk", exact:"origin") |
    remote_bookmarks(exact:"main", exact:"upstream") |
    remote_bookmarks(exact:"master", exact:"upstream") |
    remote_bookmarks(exact:"trunk", exact:"upstream") |
    root()
)"#;
const DEFAULT_BUILTIN_IMMUTABLE_HEADS_ALIAS: &str =
    "trunk() | tags() | untracked_remote_bookmarks()";
const DEFAULT_IMMUTABLE_HEADS_ALIAS: &str = "builtin_immutable_heads()";

/// Try to load the user's jj revset-aliases from their config files.
/// Returns overrides for the aliases map, if any were found.
fn load_user_revset_aliases(aliases_map: &mut RevsetAliasesMap) {
    // Check standard jj config locations
    let config_paths: Vec<std::path::PathBuf> = [
        std::env::var("JJ_CONFIG")
            .ok()
            .map(std::path::PathBuf::from),
        dirs::config_dir().map(|d| d.join("jj").join("config.toml")),
        dirs::home_dir().map(|d| d.join(".jjconfig.toml")),
    ]
    .into_iter()
    .flatten()
    .collect();

    for path in config_paths {
        let Some(table) = cached_config_table(&path) else {
            continue;
        };
        let Some(aliases) = table.get("revset-aliases").and_then(|v| v.as_table()) else {
            continue;
        };
        for (key, value) in aliases {
            if let Some(defn) = value.as_str() {
                let _ = aliases_map.insert(key, defn);
            }
        }
    }
}

/// Evaluate a revset expression with jj-cli's default aliases plus the
/// user's overrides, passing the resulting revset to `f`.
///
/// Returns `None` if the expression fails to parse, resolve, or evaluate.
fn with_evaluated_revset<R>(
    repo: &Arc<jj_lib::repo::ReadonlyRepo>,
    workspace_name: &WorkspaceName,
    expr: &str,
    f: impl FnOnce(&dyn jj_lib::revset::Revset) -> R,
) -> Option<R> {
    // Build aliases map with defaults from jj-cli
    let mut aliases_map = RevsetAliasesMap::new();
    let _ = aliases_map.insert("trunk()", DEFAULT_TRUNK_ALIAS);
    let _ = aliases_map.insert(
        "builtin_immutable_heads()",
        DEFAULT_BUILTIN_IMMUTABLE_HEADS_ALIAS,
    );
    let _ = aliases_map.insert("immutable_heads()", DEFAULT_IMMUTABLE_HEADS_ALIAS);

    // Load user overrides (e.g. custom trunk() or immutable_heads())
    load_user_revset_aliases(&mut aliases_map);

    let extensions = RevsetExtensions::new();
    let fileset_aliases = FilesetAliasesMap::new();
    let repo_path_converter = jj_lib::repo_path::RepoPathUiConverter::Fs {
        cwd: std::path::PathBuf::new(),
        base: std::path::PathBuf::new(),
    };
    let ws_context = RevsetWorkspaceContext {
        path_converter: &repo_path_converter,
        workspace_name,
    };

    let context = RevsetParseContext {
        aliases_map: &aliases_map,
        local_variables: Default::default(),
        user_email: "",
        date_pattern_context: DatePatternContext::from(chrono::Local::now()),
        default_ignored_remote: Some(RemoteName::new("git")),
        fileset_aliases_map: &fileset_aliases,
        use_glob_by_default: false,
        extensions: &extensions,
        workspace: Some(ws_context),
    };

    let mut diagnostics = RevsetDiagnostics::new();
    let expression = revset::parse(&mut diagnostics, expr, &context).ok()?;

    let symbol_resolver = SymbolResolver::new(repo.as_ref(), extensions.symbol_resolvers());
    let resolved = expression
        .resolve_user_expression(repo.as_ref(), &symbol_resolver)
        .ok()?;

    let revset = resolved.evaluate(repo.as_ref()).ok()?;
    Some(f(revset.as_ref()))
}

/// Check if a commit is immutable by evaluating the `immutable_heads()::` revset.
///
/// This uses jj's revset engine with the same default aliases as jj-cli,
/// plus any user overrides from their jj config files.
fn is_commit_immutable(
    repo: &Arc<jj_lib::repo::ReadonlyRepo>,
    workspace_name: &WorkspaceName,
    commit_id: &CommitId,
) -> bool {
    with_evaluated_revset(repo, workspace_name, "::immutable_heads()", |revset| {
        let containing = revset.containing_fn();
        containing(commit_id).unwrap_or(false)
    })
    .unwrap_or(false)
}

/// Walk ancestors via BFS to find bookmarks within `max_depth` commits.
///
/// Instead of calling `local_bookmarks_for_commit` at every BFS level (which
/// scans all bookmarks each time), we collect all bookmark target commit IDs
/// upfront into a HashMap, then do a single ancestor walk checking membership.
/// Classify the tracking status of a local bookmark vs its remote counterpart.
fn classify_tracking(
    repo: &Arc<jj_lib::repo::ReadonlyRepo>,
    local_id: &CommitId,
    remote_id: &CommitId,
) -> TrackingStatus {
    if local_id == remote_id {
        return TrackingStatus::Tracked;
    }
    let index = repo.index();
    let local_is_ancestor = index.is_ancestor(local_id, remote_id).unwrap_or(false);
    let remote_is_ancestor = index.is_ancestor(remote_id, local_id).unwrap_or(false);
    match (local_is_ancestor, remote_is_ancestor) {
        (true, false) => TrackingStatus::Behind, // local is ancestor of remote
        (false, true) => TrackingStatus::Ahead,  // remote is ancestor of local
        _ => TrackingStatus::Sideways,
    }
}

/// Build a map of bookmark name → tracking status for the given names by
/// scanning tracked remote refs. Classification costs up to two ancestry
/// walks per ref, so callers restrict it to the bookmarks actually shown
/// rather than every bookmark in the repo.
fn compute_tracking_statuses(
    repo: &Arc<jj_lib::repo::ReadonlyRepo>,
    view: &jj_lib::view::View,
    names: &HashSet<&str>,
) -> HashMap<String, TrackingStatus> {
    let mut result: HashMap<String, TrackingStatus> = HashMap::new();

    for (symbol, remote_ref) in view.all_remote_bookmarks() {
        if !names.contains(symbol.name.as_str()) {
            continue;
        }
        if !remote_ref.is_tracked() || !remote_ref.is_present() {
            continue;
        }
        let Some(remote_id) = remote_ref.target.as_normal() else {
            continue;
        };
        let name = symbol.name.as_str().to_string();
        // Look up the local bookmark target
        let local_target = view.get_local_bookmark(symbol.name);
        let Some(local_id) = local_target.as_normal() else {
            continue;
        };
        let status = classify_tracking(repo, local_id, remote_id);
        // If multiple remotes, escalate: Diverged > Ahead/Behind > Tracked
        result
            .entry(name)
            .and_modify(|existing| {
                // Keep the "worst" status
                match (&existing, &status) {
                    (TrackingStatus::Sideways, _) => {}
                    (_, TrackingStatus::Sideways) => *existing = TrackingStatus::Sideways,
                    (TrackingStatus::Ahead, TrackingStatus::Behind)
                    | (TrackingStatus::Behind, TrackingStatus::Ahead) => {
                        *existing = TrackingStatus::Sideways
                    }
                    (TrackingStatus::Tracked, _) => *existing = status.clone(),
                    _ => {}
                }
            })
            .or_insert(status);
    }

    result
}

fn find_ancestor_bookmarks(
    repo: &Arc<jj_lib::repo::ReadonlyRepo>,
    view: &jj_lib::view::View,
    workspace_name: &WorkspaceName,
    wc_id: &CommitId,
    max_depth: u32,
) -> Result<Vec<Bookmark>> {
    // Build a map from commit_id -> list of bookmark names, scanning bookmarks once.
    let mut bookmark_targets: HashMap<CommitId, Vec<String>> = HashMap::new();
    for (name, target) in view.local_bookmarks() {
        if let Some(id) = target.as_normal() {
            bookmark_targets
                .entry(id.clone())
                .or_default()
                .push(name.as_str().to_string());
        }
    }

    // Prefer bookmarks on commits in `::@ ~ ::trunk()` — the current line
    // of work. Bookmarks in trunk's history (main, master, ...) are noise
    // when feature bookmarks exist. If the current line has NO bookmarks,
    // fall back to the nearest ancestor bookmarks — typically trunk's own
    // (`main+N`) — so a fresh stack of changes still shows where it sits.
    // With no remotes, trunk() falls back to root(), so local-only repos
    // are unaffected.
    //
    // The revset is evaluated once; jj's default engine computes the
    // ancestor difference with a generation-bounded walk that visits
    // roughly `trunk()..@`, not the whole repo. If evaluation fails
    // (unusual store, broken user alias), the fallback walk runs instead.
    let expr = format!("::{} ~ ::trunk()", wc_id.hex());
    let filtered = with_evaluated_revset(repo, workspace_name, &expr, |revset| {
        let containing = revset.containing_fn();
        collect_bookmarks_bfs(
            repo,
            &bookmark_targets,
            wc_id,
            max_depth,
            &|id| containing(id).unwrap_or(true),
            false,
        )
    });
    let mut bookmarks = match filtered {
        Some(Ok(found)) if !found.is_empty() => found,
        Some(Err(e)) => return Err(e),
        // Nothing on the current line: nearest ancestor bookmarks only (stop
        // at the first depth with a match, so `main+N` shows without dragging
        // in every stale bookmark behind it).
        _ => collect_bookmarks_bfs(repo, &bookmark_targets, wc_id, max_depth, &|_| true, true)?,
    };

    // Classify tracking only for the bookmarks that will be displayed.
    let names: HashSet<&str> = bookmarks.iter().map(|b| b.name.as_str()).collect();
    let tracking_statuses = compute_tracking_statuses(repo, view, &names);
    for bookmark in &mut bookmarks {
        if let Some(status) = tracking_statuses.get(&bookmark.name) {
            bookmark.tracking = status.clone();
        }
    }
    Ok(bookmarks)
}

/// BFS over ancestors of `wc_id` up to `max_depth`, collecting bookmarks on
/// commits for which `in_branch` returns true. A commit outside the branch
/// prunes its whole subtree (its ancestors are outside too), so e.g. the
/// trunk leg of a megamerge terminates the walk immediately.
///
/// With `nearest_only`, the walk stops after the first depth level that
/// yielded any bookmark (BFS is level-ordered, so all same-distance
/// bookmarks are still collected).
fn collect_bookmarks_bfs(
    repo: &Arc<jj_lib::repo::ReadonlyRepo>,
    bookmark_targets: &HashMap<CommitId, Vec<String>>,
    wc_id: &CommitId,
    max_depth: u32,
    in_branch: &dyn Fn(&CommitId) -> bool,
    nearest_only: bool,
) -> Result<Vec<Bookmark>> {
    let mut queue: VecDeque<(CommitId, u32)> = VecDeque::new();
    let mut visited = HashSet::new();
    let mut seen_names = HashSet::new();
    let mut bookmarks = Vec::new();

    // Check bookmarks directly on the working copy commit (distance 0)
    if let Some(names) = bookmark_targets.get(wc_id)
        && in_branch(wc_id)
    {
        for name_str in names {
            if seen_names.insert(name_str.clone()) {
                bookmarks.push(Bookmark {
                    name: name_str.clone(),
                    distance: 0,
                    display: name_str.clone(),
                    tracking: TrackingStatus::default(),
                });
            }
        }
    }

    // Start BFS from WC commit's parents
    let wc_commit = repo.store().get_commit(wc_id).context("get wc commit")?;
    for parent_id in wc_commit.parent_ids() {
        queue.push_back((parent_id.clone(), 1));
    }

    while let Some((commit_id, depth)) = queue.pop_front() {
        // BFS is level-ordered: once a level has produced bookmarks, any
        // deeper node means that level is exhausted.
        if nearest_only
            && let Some(first) = bookmarks.first()
            && depth > first.distance
        {
            break;
        }
        if depth > max_depth || !visited.insert(commit_id.clone()) {
            continue;
        }

        if !in_branch(&commit_id) {
            continue;
        }

        if let Some(names) = bookmark_targets.get(&commit_id) {
            for name_str in names {
                if seen_names.insert(name_str.clone()) {
                    let display = format!("{name_str}+{depth}");
                    bookmarks.push(Bookmark {
                        name: name_str.clone(),
                        distance: depth,
                        display,
                        tracking: TrackingStatus::default(),
                    });
                }
            }
        }

        if depth < max_depth {
            let commit = repo
                .store()
                .get_commit(&commit_id)
                .context("get ancestor commit")?;
            for parent_id in commit.parent_ids() {
                queue.push_back((parent_id.clone(), depth + 1));
            }
        }
    }

    Ok(bookmarks)
}

/// Core jj-lib query logic. Returns both the status and retained state for incremental updates.
/// Loaded jj workspace state — shared by full refresh and validate-and-refresh paths.
struct JjLoadedRepo {
    repo: Arc<jj_lib::repo::ReadonlyRepo>,
    commit: jj_lib::commit::Commit,
    /// Parent tree (merged for merge commits); `None` if it failed to load.
    /// Retained so `compute_jj_full_status` doesn't redo the merge.
    parent_tree: Option<jj_lib::merged_tree::MergedTree>,
    parent_tree_ids: jj_lib::merge::Merge<jj_lib::backend::TreeId>,
    /// Metadata-only status (no diff stats populated).
    metadata_status: RepoStatus,
}

/// Retained workspace loader, reused across refreshes so the Store's
/// commit/tree caches stay warm instead of loading cold each time.
///
/// Reuse is sound because `RepoLoader::load_at_head` re-reads op heads from
/// disk on every call (new operations are always observed) and the Store
/// caches are keyed by immutable content-addressed ids. Commits written by
/// external processes are healed by the git backend's own reload-on-miss
/// path. Anything else that goes stale (repo deleted/recreated, `.jj`
/// replaced) surfaces as a load error, which callers handle by dropping the
/// loader and retrying cold.
struct JjWorkspaceLoader {
    workspace_name: jj_lib::ref_name::WorkspaceNameBuf,
    repo_loader: jj_lib::repo::RepoLoader,
}

/// Load a workspace from disk and keep the pieces needed for repeat loads.
fn load_workspace_loader(repo_path: &Path) -> Result<JjWorkspaceLoader> {
    let settings = create_user_settings()?;
    let workspace = Workspace::load(
        &settings,
        repo_path,
        &StoreFactories::default(),
        &default_working_copy_factories(),
    )
    .context("load jj workspace")?;
    Ok(JjWorkspaceLoader {
        workspace_name: workspace.workspace_name().to_owned(),
        repo_loader: workspace.repo_loader().clone(),
    })
}

/// Load a jj workspace/repo and compute metadata. This is the shared first step
/// for both full refresh and validate-and-refresh — call once, then branch on
/// whether the parent tree IDs changed.
///
/// One-shot cold load; workers with retained state use `load_jj_repo_with`.
async fn load_jj_repo(repo_path: &Path, depth: u32) -> Result<JjLoadedRepo> {
    let loader = load_workspace_loader(repo_path)?;
    load_jj_repo_with(&loader, depth).await
}

/// Load the repo at the current op head through a (possibly retained)
/// loader and compute metadata.
async fn load_jj_repo_with(loader: &JjWorkspaceLoader, depth: u32) -> Result<JjLoadedRepo> {
    let workspace_name = loader.workspace_name.clone();
    let repo: Arc<jj_lib::repo::ReadonlyRepo> = loader
        .repo_loader
        .load_at_head()
        .await
        .context("load jj repo at head")?;

    let view = repo.view();
    let wc_id = view
        .get_wc_commit_id(&workspace_name)
        .context("no working copy commit for workspace")?
        .clone();

    let commit = repo
        .store()
        .get_commit(&wc_id)
        .context("get working copy commit")?;

    let parent_tree = commit.parent_tree(repo.as_ref()).await.ok();
    let parent_tree_ids = parent_tree
        .as_ref()
        .map(|t| t.tree_ids().clone())
        .unwrap_or_else(|| commit.tree_ids().clone());

    // Compute metadata-only status (no diff stats)
    let mut status = RepoStatus {
        is_jj: true,
        ..Default::default()
    };

    let change_id_full = encode_reverse_hex(commit.change_id().as_bytes());
    let id_len = 8.min(change_id_full.len());
    status.change_id = change_id_full[..id_len].to_string();
    status.change_id_prefix_len = repo
        .shortest_unique_change_id_prefix_len(commit.change_id())
        .unwrap_or(id_len);

    let commit_id_hex = commit.id().hex();
    let id_len = 8.min(commit_id_hex.len());
    status.commit_id = commit_id_hex[..id_len].to_string();
    status.commit_id_prefix_len = repo
        .index()
        .shortest_unique_commit_id_prefix_len(commit.id())
        .unwrap_or(id_len);

    status.description = commit
        .description()
        .lines()
        .next()
        .unwrap_or("")
        .to_string();

    status.conflict = commit.has_conflict();
    status.divergent = repo
        .resolve_change_id(commit.change_id())
        .ok()
        .flatten()
        .is_some_and(|targets| targets.visible_with_offsets().count() > 1);
    status.hidden = commit.is_hidden(repo.as_ref()).unwrap_or(false);
    status.immutable = is_commit_immutable(&repo, &workspace_name, &wc_id);
    status.bookmarks = find_ancestor_bookmarks(&repo, view, &workspace_name, &wc_id, depth)?;
    status.workspace_name = workspace_name.as_str().to_string();
    status.is_default_workspace = status.workspace_name == "default";

    Ok(JjLoadedRepo {
        repo,
        commit,
        parent_tree,
        parent_tree_ids,
        metadata_status: status,
    })
}

/// Given an already-loaded repo, compute full diff stats and build JjRepoState.
async fn compute_jj_full_status(
    repo_path: &Path,
    loaded: JjLoadedRepo,
) -> Result<(RepoStatus, JjRepoState)> {
    let mut status = loaded.metadata_status;
    let conflict_marker_style = conflict_marker_style_for_repo(repo_path);

    let parent_tree = loaded.parent_tree;
    let current_tree = loaded.commit.tree();
    let base_file_stats = if let Some(ref parent_tree) = parent_tree {
        let copy_records = gather_copy_records(loaded.repo.store(), &loaded.commit).await;
        let per_file = compute_per_file_diff_stats(
            loaded.repo.store(),
            parent_tree,
            &current_tree,
            conflict_marker_style,
            &copy_records,
        )
        .await;
        let c = aggregate_file_stats(&per_file);
        status.file_mad_count_working_tree = c.file_mad_count;
        status.lines_added_working_tree = c.lines_added;
        status.lines_removed_working_tree = c.lines_removed;
        status.files_modified_working_tree = c.files_modified;
        status.files_added_working_tree = c.files_added;
        status.files_deleted_working_tree = c.files_deleted;
        status.file_mad_count = c.file_mad_count;
        status.lines_added_total = c.lines_added;
        status.lines_removed_total = c.lines_removed;
        status.files_modified_total = c.files_modified;
        status.files_added_total = c.files_added;
        status.files_deleted_total = c.files_deleted;
        status.empty = c.file_mad_count == 0;
        per_file
    } else {
        status.empty = true;
        HashMap::new()
    };

    let repo_root = repo_path
        .canonicalize()
        .unwrap_or_else(|_| repo_path.to_path_buf());

    let commit_tree = current_tree.clone();
    // Conflicted paths in the WC commit, for the writing-snapshot
    // simplification mirror (see `JjRepoState::conflicted_paths`).
    let conflicted_paths: Vec<jj_lib::repo_path::RepoPathBuf> = if commit_tree.has_conflict() {
        commit_tree.conflicts().map(|(path, _)| path).collect()
    } else {
        Vec::new()
    };
    let retained_parent_tree = parent_tree.unwrap_or(current_tree);
    // Individual parent trees for per-parent copy-detection decisions,
    // projected to the first term of each root-tree merge — the tree gix
    // actually reads (GitBackend::read_tree_for_commit). Failures degrade
    // to "no per-parent knowledge" — single-parent behavior.
    let parent_trees = match loaded.commit.parents().await {
        Ok(parents) => parents
            .iter()
            .map(|parent| {
                jj_lib::merged_tree::MergedTree::new(
                    loaded.repo.store().clone(),
                    jj_lib::merge::Merge::resolved(parent.tree_ids().first().clone()),
                    jj_lib::conflict_labels::ConflictLabels::unlabeled(),
                )
            })
            .collect(),
        Err(_) => Vec::new(),
    };

    let jj_state = JjRepoState {
        store: loaded.repo.store().clone(),
        parent_tree_ids: retained_parent_tree.tree_ids().clone(),
        parent_tree: retained_parent_tree,
        commit_tree,
        parent_trees,
        op_id: loaded.repo.op_id().clone(),
        conflict_marker_style,
        ignore_filter: crate::watcher::IgnoreFilter::new(&repo_root, crate::protocol::VcsKind::Jj),
        conflicted_paths,
        base_file_stats,
        overlay: HashMap::new(),
        repo_root,
        base_status: status.clone(),
    };

    Ok((status, jj_state))
}

/// Copy metadata fields from a freshly computed status into the retained base_status.
/// Does NOT touch diff stats fields — those are managed by the overlay.
///
/// **Keep in sync with `RepoStatus`**: if you add a metadata field to `RepoStatus`,
/// you must add it here too. Diff stats fields (lines_added_*, file_mad_count_*, etc.)
/// and `empty` (derived from diffs for jj, from commit tree for git) should NOT be
/// copied here — they are maintained by the incremental diff overlay.
fn update_base_status_metadata(base: &mut RepoStatus, fresh: &RepoStatus) {
    base.change_id.clone_from(&fresh.change_id);
    base.change_id_prefix_len = fresh.change_id_prefix_len;
    base.commit_id.clone_from(&fresh.commit_id);
    base.commit_id_prefix_len = fresh.commit_id_prefix_len;
    base.description.clone_from(&fresh.description);
    base.conflict = fresh.conflict;
    base.divergent = fresh.divergent;
    base.hidden = fresh.hidden;
    base.immutable = fresh.immutable;
    base.bookmarks.clone_from(&fresh.bookmarks);
    base.workspace_name.clone_from(&fresh.workspace_name);
    base.is_default_workspace = fresh.is_default_workspace;
}

/// This produces `!Send` futures (due to jj-lib internals),
/// so it must be run via `block_on` inside `spawn_blocking`.
#[tracing::instrument(fields(repo = %repo_path.display()))]
async fn query_jj_lib(repo_path: &Path, depth: u32) -> Result<(RepoStatus, JjRepoState)> {
    let loaded = load_jj_repo(repo_path, depth).await?;
    compute_jj_full_status(repo_path, loaded).await
}

/// Load via the retained loader when present; on failure drop it and retry
/// once with a cold load. (Re)fills `cached` on success. The returned flag
/// is true when the load went through the retained loader.
async fn load_jj_repo_cached(
    cached: &mut Option<JjWorkspaceLoader>,
    repo_path: &Path,
    depth: u32,
) -> Result<(JjLoadedRepo, bool)> {
    if let Some(loader) = cached.as_ref() {
        match load_jj_repo_with(loader, depth).await {
            Ok(loaded) => return Ok((loaded, true)),
            Err(e) => {
                tracing::warn!(
                    repo = %repo_path.display(),
                    error = %e,
                    "retained jj loader failed, retrying with cold load"
                );
                *cached = None;
            }
        }
    }
    let loader = load_workspace_loader(repo_path)?;
    let loaded = load_jj_repo_with(&loader, depth).await?;
    *cached = Some(loader);
    Ok((loaded, false))
}

/// Full refresh through the retained loader. An error in the diff stage
/// after a retained load may also be staleness (the store handles are
/// shared), so it gets the same drop-and-retry-cold treatment as load
/// errors before being reported.
async fn query_jj_cached(
    cached: &mut Option<JjWorkspaceLoader>,
    repo_path: &Path,
    depth: u32,
) -> Result<(RepoStatus, JjRepoState)> {
    let (loaded, used_retained) = load_jj_repo_cached(cached, repo_path, depth).await?;
    match compute_jj_full_status(repo_path, loaded).await {
        Err(e) if used_retained => {
            tracing::warn!(
                repo = %repo_path.display(),
                error = %e,
                "full status failed via retained loader, retrying with cold load"
            );
            *cached = None;
            let (loaded, _) = load_jj_repo_cached(cached, repo_path, depth).await?;
            compute_jj_full_status(repo_path, loaded).await
        }
        result => result,
    }
}

#[tracing::instrument(skip(config), fields(repo = %repo_path.display()))]
pub async fn query_jj_status(repo_path: &Path, config: &Config) -> Result<RepoStatus> {
    let (status, _state) = query_jj_status_with_state(repo_path, config).await?;
    Ok(status)
}

/// Query jj status and return retained state for incremental updates.
#[tracing::instrument(skip(config), fields(repo = %repo_path.display()))]
pub async fn query_jj_status_with_state(
    repo_path: &Path,
    config: &Config,
) -> Result<(RepoStatus, JjRepoState)> {
    let repo_path = repo_path.to_path_buf();
    let depth = config.bookmark_search_depth;

    let handle = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || handle.block_on(query_jj_lib(&repo_path, depth)))
        .await
        .context("jj-lib task panicked")?
}

/// Requests that can be sent to the jj worker thread.
pub enum JjWorkerRequest {
    /// Full refresh: reload workspace/repo, compute all status fields.
    FullRefresh {
        repo_path: PathBuf,
        depth: u32,
        reply: tokio::sync::oneshot::Sender<Result<RepoStatus>>,
    },
    /// Validate baseline and refresh: reload workspace/repo, check if parent tree
    /// changed. If unchanged, do metadata-only update (and optional incremental WC
    /// diffs). If changed, fall through to full refresh.
    ValidateAndRefresh {
        repo_path: PathBuf,
        changed_paths: Vec<PathBuf>,
        depth: u32,
        reply: tokio::sync::oneshot::Sender<Result<RepoStatus>>,
    },
    /// Incremental update: diff specific working copy files against parent tree.
    IncrementalUpdate {
        repo_path: PathBuf,
        changed_paths: Vec<PathBuf>,
        reply: tokio::sync::oneshot::Sender<Result<RepoStatus>>,
    },
    /// Full resync after watcher event loss (OS queue overflow): rebuild all
    /// state from the store, then re-diff previously-known dirty files so
    /// unsnapshotted disk edits aren't forgotten.
    Resync {
        repo_path: PathBuf,
        depth: u32,
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
    /// Drop the retained state and worker thread for a repo (e.g. the repo
    /// was deleted). The worker thread exits once its channel closes.
    Forget { repo_path: PathBuf },
}

/// Spawn the jj worker router thread.
///
/// Returns a sender for submitting requests. The router forwards each
/// request to a lazily spawned per-repo worker thread that owns that repo's
/// !Send jj-lib state, so a long full refresh in one repo never blocks
/// refreshes in another. The router and its workers exit when the returned
/// sender is dropped.
pub fn spawn_jj_worker() -> mpsc::UnboundedSender<JjWorkerRequest> {
    let (tx, rx) = mpsc::unbounded_channel();
    let handle = tokio::runtime::Handle::current();
    std::thread::spawn(move || jj_router_loop(rx, handle));
    tx
}

fn jj_router_loop(
    mut rx: mpsc::UnboundedReceiver<JjWorkerRequest>,
    handle: tokio::runtime::Handle,
) {
    let mut workers: HashMap<PathBuf, mpsc::UnboundedSender<JjWorkerRequest>> = HashMap::new();

    while let Some(req) = rx.blocking_recv() {
        match req {
            JjWorkerRequest::Forget { repo_path } => {
                // Dropping the sender closes the worker's channel; the thread
                // exits after finishing any in-flight request.
                workers.remove(&repo_path);
            }
            // Stats requests fan out to every repo worker. The gathering runs
            // on an ephemeral thread so a repo mid-refresh delays only its
            // own entry, never the router.
            JjWorkerRequest::QueryOverlayStats { reply } => {
                let snapshot: Vec<_> = workers.values().cloned().collect();
                std::thread::spawn(move || {
                    let mut pending = Vec::new();
                    for worker in snapshot {
                        let (tx, rx) = tokio::sync::oneshot::channel();
                        if worker
                            .send(JjWorkerRequest::QueryOverlayStats { reply: tx })
                            .is_ok()
                        {
                            pending.push(rx);
                        }
                    }
                    let mut all = Vec::new();
                    for rx in pending {
                        if let Ok(stats) = rx.blocking_recv() {
                            all.extend(stats);
                        }
                    }
                    let _ = reply.send(all);
                });
            }
            JjWorkerRequest::QueryOverlayStatsVerbose { reply } => {
                let snapshot: Vec<_> = workers.values().cloned().collect();
                std::thread::spawn(move || {
                    let mut pending = Vec::new();
                    for worker in snapshot {
                        let (tx, rx) = tokio::sync::oneshot::channel();
                        if worker
                            .send(JjWorkerRequest::QueryOverlayStatsVerbose { reply: tx })
                            .is_ok()
                        {
                            pending.push(rx);
                        }
                    }
                    let mut all = Vec::new();
                    for rx in pending {
                        if let Ok(stats) = rx.blocking_recv() {
                            all.extend(stats);
                        }
                    }
                    let _ = reply.send(all);
                });
            }
            req => {
                let repo_path = match &req {
                    JjWorkerRequest::FullRefresh { repo_path, .. }
                    | JjWorkerRequest::ValidateAndRefresh { repo_path, .. }
                    | JjWorkerRequest::IncrementalUpdate { repo_path, .. }
                    | JjWorkerRequest::Resync { repo_path, .. } => repo_path.clone(),
                    JjWorkerRequest::Forget { .. }
                    | JjWorkerRequest::QueryOverlayStats { .. }
                    | JjWorkerRequest::QueryOverlayStatsVerbose { .. } => {
                        unreachable!("handled above")
                    }
                };
                // A send fails only if the worker thread died (panicked);
                // drop the stale sender and respawn for this repo.
                let req = match workers.get(&repo_path) {
                    Some(worker) => match worker.send(req) {
                        Ok(()) => continue,
                        Err(tokio::sync::mpsc::error::SendError(req)) => {
                            workers.remove(&repo_path);
                            req
                        }
                    },
                    None => req,
                };
                let worker = spawn_repo_worker(handle.clone());
                let _ = worker.send(req);
                workers.insert(repo_path, worker);
            }
        }
    }
}

/// Spawn a dedicated thread that owns a single repo's !Send jj-lib state.
fn spawn_repo_worker(handle: tokio::runtime::Handle) -> mpsc::UnboundedSender<JjWorkerRequest> {
    let (tx, rx) = mpsc::unbounded_channel();
    std::thread::spawn(move || {
        handle.block_on(repo_worker_loop(rx));
    });
    tx
}

async fn repo_worker_loop(mut rx: mpsc::UnboundedReceiver<JjWorkerRequest>) {
    // The daemon-facing repo path (the router's map key), used to label
    // overlay stats; `JjRepoState.repo_root` is canonicalized and may differ.
    let mut worker_repo_path: Option<PathBuf> = None;
    let mut repo_state: Option<JjRepoState> = None;
    // Workspace loader retained across refreshes to keep store caches warm.
    let mut cached_loader: Option<JjWorkspaceLoader> = None;

    while let Some(req) = rx.recv().await {
        match req {
            JjWorkerRequest::FullRefresh {
                repo_path,
                depth,
                reply,
            } => {
                worker_repo_path = Some(repo_path.clone());
                let result = query_jj_cached(&mut cached_loader, &repo_path, depth).await;
                match result {
                    Ok((status, jj_state)) => {
                        repo_state = Some(jj_state);
                        let _ = reply.send(Ok(status));
                    }
                    Err(e) => {
                        let _ = reply.send(Err(e));
                    }
                }
            }
            JjWorkerRequest::ValidateAndRefresh {
                repo_path,
                changed_paths,
                depth,
                reply,
            } => {
                worker_repo_path = Some(repo_path.clone());
                let Some(state) = repo_state.as_mut() else {
                    // No retained state — fall through to full refresh
                    let result = query_jj_cached(&mut cached_loader, &repo_path, depth).await;
                    match result {
                        Ok((status, jj_state)) => {
                            repo_state = Some(jj_state);
                            let _ = reply.send(Ok(status));
                        }
                        Err(e) => {
                            let _ = reply.send(Err(e));
                        }
                    }
                    continue;
                };

                // Load workspace/repo once — then branch on parent tree IDs
                let (loaded, used_retained) =
                    match load_jj_repo_cached(&mut cached_loader, &repo_path, depth).await {
                        Ok(l) => l,
                        Err(e) => {
                            let _ = reply.send(Err(e));
                            continue;
                        }
                    };

                let trees_unchanged = loaded.parent_tree_ids == state.parent_tree_ids
                    && loaded.commit.tree_ids() == state.commit_tree.tree_ids();

                if trees_unchanged {
                    // Neither the parent tree nor the WC commit tree changed —
                    // metadata-only update. Keep base_file_stats and overlay.
                    tracing::debug!(
                        repo = %repo_path.display(),
                        "parent and commit trees unchanged — metadata-only refresh"
                    );
                    if loaded.repo.op_id() != &state.op_id {
                        // The op log advanced but the trees ended up where
                        // they started (A→B→A, e.g. abandon snapshotting and
                        // then discarding a dirty file). The op may have
                        // rewritten working-copy files whose events were
                        // lost — re-diff everything we believe is dirty.
                        state.op_id = loaded.repo.op_id().clone();
                        let dirty: Vec<PathBuf> = state
                            .overlay
                            .keys()
                            .map(|rel| state.repo_root.join(rel))
                            .collect();
                        apply_incremental_paths(state, &dirty).await;
                    }
                    update_base_status_metadata(&mut state.base_status, &loaded.metadata_status);
                    apply_incremental_paths(state, &changed_paths).await;
                    let status = state.current_status();
                    let _ = reply.send(Ok(status));
                } else {
                    // A tree changed: the op moved @ (parent changed) or
                    // rewrote/snapshotted the WC commit (abandon, restore,
                    // edit-to-sibling, undo, snapshot). Rebuild the whole
                    // state from the store — this also self-heals any drift
                    // from dropped watcher events. Disk writes racing after
                    // the op's snapshot are re-applied from changed_paths.
                    tracing::debug!(
                        repo = %repo_path.display(),
                        "parent or commit tree changed — full refresh"
                    );
                    let result = match compute_jj_full_status(&repo_path, loaded).await {
                        Err(e) if used_retained => {
                            // Same staleness handling as query_jj_cached:
                            // drop the loader and redo the refresh cold.
                            tracing::warn!(
                                repo = %repo_path.display(),
                                error = %e,
                                "full status failed via retained loader, retrying with cold load"
                            );
                            cached_loader = None;
                            query_jj_cached(&mut cached_loader, &repo_path, depth).await
                        }
                        result => result,
                    };
                    match result {
                        Ok((_, mut jj_state)) => {
                            apply_incremental_paths(&mut jj_state, &changed_paths).await;
                            let status = jj_state.current_status();
                            repo_state = Some(jj_state);
                            let _ = reply.send(Ok(status));
                        }
                        Err(e) => {
                            let _ = reply.send(Err(e));
                        }
                    }
                }
            }
            JjWorkerRequest::IncrementalUpdate {
                repo_path,
                changed_paths,
                reply,
            } => {
                worker_repo_path = Some(repo_path.clone());
                let Some(state) = repo_state.as_mut() else {
                    // No retained state — caller should do a full refresh instead
                    let _ = reply.send(Err(anyhow::anyhow!("no incremental state for repo")));
                    continue;
                };
                let _span = tracing::debug_span!("incremental_update",
                    repo = %repo_path.display(),
                    files = changed_paths.len()
                )
                .entered();

                apply_incremental_paths(state, &changed_paths).await;

                let status = state.current_status();
                let _ = reply.send(Ok(status));
            }
            JjWorkerRequest::Resync {
                repo_path,
                depth,
                reply,
            } => {
                // Events were lost — every part of the cached state is
                // suspect. Rebuild from the store, then re-diff the files we
                // previously knew were dirty (overlay keys); disk changes we
                // never heard about at all can only be healed by the next op.
                worker_repo_path = Some(repo_path.clone());
                // Drop the retained loader too: resync means our picture of
                // the repo is untrustworthy, and the cold load doubles as a
                // pressure valve for accumulated store caches.
                cached_loader = None;
                let prior_dirty: Vec<PathBuf> = repo_state
                    .as_ref()
                    .map(|s| s.overlay.keys().map(|rel| s.repo_root.join(rel)).collect())
                    .unwrap_or_default();
                match query_jj_cached(&mut cached_loader, &repo_path, depth).await {
                    Ok((_, mut jj_state)) => {
                        apply_incremental_paths(&mut jj_state, &prior_dirty).await;
                        let status = jj_state.current_status();
                        repo_state = Some(jj_state);
                        let _ = reply.send(Ok(status));
                    }
                    Err(e) => {
                        let _ = reply.send(Err(e));
                    }
                }
            }
            JjWorkerRequest::QueryOverlayStats { reply } => {
                let stats = match (&worker_repo_path, &repo_state) {
                    (Some(path), Some(state)) => {
                        let counts = state.aggregate_stats();
                        vec![(
                            path.to_string_lossy().to_string(),
                            crate::protocol::IncrementalDiffStats {
                                base_files: state.base_file_stats.len() as u32,
                                overlay_entries: state.overlay.len() as u32,
                                files_changed: counts.file_mad_count,
                                lines_added: counts.lines_added,
                                lines_removed: counts.lines_removed,
                            },
                        )]
                    }
                    _ => Vec::new(),
                };
                let _ = reply.send(stats);
            }
            JjWorkerRequest::QueryOverlayStatsVerbose { reply } => {
                let stats = match (&worker_repo_path, &repo_state) {
                    (Some(path), Some(state)) => {
                        let dir_stats =
                            aggregate_overlay_stats_by_dir(&state.base_file_stats, &state.overlay);
                        vec![(path.to_string_lossy().to_string(), dir_stats)]
                    }
                    _ => Vec::new(),
                };
                let _ = reply.send(stats);
            }
            // The router intercepts Forget; exit defensively if one slips
            // through so the thread can't outlive its repo.
            JjWorkerRequest::Forget { .. } => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use tokio::process::Command;

    use crate::test_util::create_jj_repo_async as create_jj_repo;

    async fn jj_cmd(repo: &Path, args: &[&str]) -> String {
        let output = Command::new("jj")
            .args(args)
            .current_dir(repo)
            .output()
            .await
            .unwrap();
        assert!(
            output.status.success(),
            "jj {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).to_string()
    }

    #[tokio::test]
    async fn test_empty_repo() {
        let dir = create_jj_repo().await;
        let config = Config {
            color: false,
            ..Default::default()
        };
        let status = query_jj_status(dir.path(), &config).await.unwrap();
        assert!(!status.change_id.is_empty());
        assert!(status.empty);
        assert!(status.bookmarks.is_empty());
    }

    #[tokio::test]
    async fn test_with_description() {
        let dir = create_jj_repo().await;
        jj_cmd(dir.path(), &["describe", "-m", "hello world"]).await;
        let config = Config {
            color: false,
            ..Default::default()
        };
        let status = query_jj_status(dir.path(), &config).await.unwrap();
        assert_eq!(status.description, "hello world");
    }

    #[tokio::test]
    async fn test_with_bookmark() {
        let dir = create_jj_repo().await;
        jj_cmd(dir.path(), &["bookmark", "create", "main", "-r", "@"]).await;
        let config = Config {
            color: false,
            ..Default::default()
        };
        let status = query_jj_status(dir.path(), &config).await.unwrap();
        assert!(
            status
                .bookmarks
                .iter()
                .any(|b| b.name == "main" && b.distance == 0 && b.display == "main")
        );
    }

    #[tokio::test]
    async fn test_bookmark_distance() {
        let dir = create_jj_repo().await;
        jj_cmd(dir.path(), &["bookmark", "create", "main", "-r", "@"]).await;
        jj_cmd(dir.path(), &["new"]).await;
        let config = Config {
            color: false,
            ..Default::default()
        };
        let status = query_jj_status(dir.path(), &config).await.unwrap();
        assert!(
            status
                .bookmarks
                .iter()
                .any(|b| b.name == "main" && b.distance == 1 && b.display == "main+1")
        );
    }

    #[tokio::test]
    async fn test_default_workspace() {
        let dir = create_jj_repo().await;
        let config = Config {
            color: false,
            ..Default::default()
        };
        let status = query_jj_status(dir.path(), &config).await.unwrap();
        assert_eq!(status.workspace_name, "default");
        assert!(status.is_default_workspace);
    }

    #[tokio::test]
    async fn test_named_workspace() {
        let dir = create_jj_repo().await;
        let work2_dir = TempDir::with_prefix("jj-ws-").unwrap();
        // jj workspace add needs a non-existing or empty dir — use a subdir of the temp
        let work2 = work2_dir.path().join("secondary");
        jj_cmd(
            dir.path(),
            &[
                "workspace",
                "add",
                "--name",
                "secondary",
                work2.to_str().unwrap(),
            ],
        )
        .await;

        let config = Config {
            color: false,
            ..Default::default()
        };

        // Query from the secondary workspace
        let status = query_jj_status(&work2, &config).await.unwrap();
        assert_eq!(status.workspace_name, "secondary");
        assert!(!status.is_default_workspace);

        // Original workspace is still "default"
        let status = query_jj_status(dir.path(), &config).await.unwrap();
        assert_eq!(status.workspace_name, "default");
        assert!(status.is_default_workspace);
    }

    /// Bookmarks on commits in trunk's history (main, master, ...) are
    /// excluded: only bookmarks in `::@ ~ ::trunk()` are shown.
    #[tokio::test]
    async fn test_bookmarks_exclude_trunk_history() {
        let dir = create_jj_repo().await;

        // Side git repo with a commit on `main`; fetching it makes
        // main@origin resolve as trunk().
        let remote = TempDir::new().unwrap();
        crate::test_util::create_git_repo_in(remote.path());
        {
            let repo = git2::Repository::open(remote.path()).unwrap();
            let head = repo.head().unwrap().peel_to_commit().unwrap();
            if repo.find_branch("main", git2::BranchType::Local).is_err() {
                repo.branch("main", &head, false).unwrap();
            }
            repo.set_head("refs/heads/main").unwrap();
        }
        jj_cmd(
            dir.path(),
            &[
                "git",
                "remote",
                "add",
                "origin",
                remote.path().to_str().unwrap(),
            ],
        )
        .await;
        jj_cmd(dir.path(), &["git", "fetch"]).await;

        // A local bookmark pointing INTO trunk history — must be hidden.
        jj_cmd(
            dir.path(),
            &["bookmark", "create", "local-main", "-r", "main@origin"],
        )
        .await;

        // Feature line on top of trunk: trunk <- feat-commit <- @
        jj_cmd(dir.path(), &["new", "main@origin"]).await;
        std::fs::write(dir.path().join("feat.txt"), "work\n").unwrap();
        jj_cmd(dir.path(), &["commit", "-m", "feat work"]).await;
        jj_cmd(dir.path(), &["bookmark", "create", "feat", "-r", "@-"]).await;

        let config = Config {
            color: false,
            ..Default::default()
        };
        let status = query_jj_status(dir.path(), &config).await.unwrap();

        assert!(
            status
                .bookmarks
                .iter()
                .any(|b| b.name == "feat" && b.distance == 1),
            "feature bookmark on the current line should be shown: {:?}",
            status.bookmarks
        );
        assert!(
            !status.bookmarks.iter().any(|b| b.name == "local-main"),
            "bookmark in trunk history should be hidden: {:?}",
            status.bookmarks
        );
    }

    /// With no bookmarks between @ and trunk, fall back to the nearest
    /// ancestor bookmark (`main+N`) — but only the nearest level, not
    /// older bookmarks further behind trunk.
    #[tokio::test]
    async fn test_bookmarks_fall_back_to_nearest_trunk_bookmark() {
        let dir = create_jj_repo().await;

        // Side remote with TWO commits on main, so trunk has a parent to
        // hang an older bookmark on.
        let remote = TempDir::new().unwrap();
        crate::test_util::create_git_repo_in(remote.path());
        {
            let repo = git2::Repository::open(remote.path()).unwrap();
            let head = repo.head().unwrap().peel_to_commit().unwrap();
            if repo.find_branch("main", git2::BranchType::Local).is_err() {
                repo.branch("main", &head, false).unwrap();
            }
            repo.set_head("refs/heads/main").unwrap();
            // second commit on main
            std::fs::write(remote.path().join("second.txt"), "two\n").unwrap();
            let mut index = repo.index().unwrap();
            index.add_path(Path::new("second.txt")).unwrap();
            index.write().unwrap();
            let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
            let sig = repo.signature().unwrap();
            let parent = repo.head().unwrap().peel_to_commit().unwrap();
            repo.commit(
                Some("refs/heads/main"),
                &sig,
                &sig,
                "second",
                &tree,
                &[&parent],
            )
            .unwrap();
        }
        jj_cmd(
            dir.path(),
            &[
                "git",
                "remote",
                "add",
                "origin",
                remote.path().to_str().unwrap(),
            ],
        )
        .await;
        jj_cmd(dir.path(), &["git", "fetch"]).await;

        // local-main at trunk, old-bm one behind trunk
        jj_cmd(
            dir.path(),
            &["bookmark", "create", "local-main", "-r", "main@origin"],
        )
        .await;
        jj_cmd(
            dir.path(),
            &["bookmark", "create", "old-bm", "-r", "main@origin-"],
        )
        .await;

        // Two unbookmarked commits on top of trunk: trunk <- c1 <- @
        jj_cmd(dir.path(), &["new", "main@origin"]).await;
        std::fs::write(dir.path().join("work.txt"), "work\n").unwrap();
        jj_cmd(dir.path(), &["commit", "-m", "work"]).await;

        let config = Config {
            color: false,
            ..Default::default()
        };
        let status = query_jj_status(dir.path(), &config).await.unwrap();

        assert!(
            status
                .bookmarks
                .iter()
                .any(|b| b.name == "local-main" && b.distance == 2),
            "nearest trunk bookmark should show as main+2: {:?}",
            status.bookmarks
        );
        assert!(
            !status.bookmarks.iter().any(|b| b.name == "old-bm"),
            "bookmarks behind the nearest level should stay hidden: {:?}",
            status.bookmarks
        );
    }

    #[tokio::test]
    async fn test_diff_stats() {
        let dir = create_jj_repo().await;
        std::fs::write(dir.path().join("test.txt"), "hello\nworld\n").unwrap();
        // Snapshot the working copy so jj-lib sees the new file
        jj_cmd(dir.path(), &["status"]).await;
        let config = Config {
            color: false,
            ..Default::default()
        };
        let status = query_jj_status(dir.path(), &config).await.unwrap();
        assert!(status.file_mad_count_working_tree >= 1);
        assert!(status.lines_added_working_tree > 0);
        // For jj, total should equal unstaged (no staging area)
        assert_eq!(status.file_mad_count, status.file_mad_count_working_tree);
        assert_eq!(status.lines_added_total, status.lines_added_working_tree);
        assert_eq!(
            status.lines_removed_total,
            status.lines_removed_working_tree
        );
        assert_eq!(status.file_mad_count_staged, 0);
    }

    /// Parse the summary line from `diff --stat` output.
    /// Handles: " 3 files changed, 10 insertions(+), 5 deletions(-)"
    use crate::test_util::parse_diff_stat_summary;

    /// Complex scenario: multiple files added, deleted, and modified with
    /// line-level changes. Verifies our diff stats match `jj diff --stat`.
    #[tokio::test]
    async fn test_diff_stats_match_jj_cli() {
        let dir = create_jj_repo().await;

        // Create initial files with known content
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::create_dir_all(dir.path().join("tests")).unwrap();

        // src/main.rs: 20 lines
        let main_initial: String = (1..=20).map(|i| format!("fn line_{i}() {{}}\n")).collect();
        std::fs::write(dir.path().join("src/main.rs"), &main_initial).unwrap();

        // src/lib.rs: 15 lines (unchanged throughout test)
        let lib_content: String = (1..=15)
            .map(|i| format!("pub fn lib_{i}() {{}}\n"))
            .collect();
        std::fs::write(dir.path().join("src/lib.rs"), &lib_content).unwrap();

        // README.md: 10 lines
        let readme_initial: String = (1..=10).map(|i| format!("# Section {i}\n")).collect();
        std::fs::write(dir.path().join("README.md"), &readme_initial).unwrap();

        // config.toml: 5 lines
        std::fs::write(
            dir.path().join("config.toml"),
            "key1 = \"val1\"\nkey2 = \"val2\"\nkey3 = \"val3\"\nkey4 = \"val4\"\nkey5 = \"val5\"\n",
        )
        .unwrap();

        // tests/test_basic.rs: 12 lines (will be deleted)
        let test_initial: String = (1..=12)
            .map(|i| format!("#[test] fn test_{i}() {{}}\n"))
            .collect();
        std::fs::write(dir.path().join("tests/test_basic.rs"), &test_initial).unwrap();

        // Commit these as the parent: `jj new` moves @ forward
        jj_cmd(dir.path(), &["new"]).await;

        // --- Complex modifications ---

        // 1. New file: src/utils.rs (8 lines)
        let utils_content: String = (1..=8)
            .map(|i| format!("pub fn util_{i}() {{}}\n"))
            .collect();
        std::fs::write(dir.path().join("src/utils.rs"), &utils_content).unwrap();

        // 2. Delete tests/test_basic.rs
        std::fs::remove_file(dir.path().join("tests/test_basic.rs")).unwrap();

        // 3. Modify src/main.rs: change lines 5-7, add 4 lines at end
        let mut main_lines: Vec<String> = (1..=20).map(|i| format!("fn line_{i}() {{}}")).collect();
        main_lines[4] = "fn modified_5() { /* changed */ }".to_string();
        main_lines[5] = "fn modified_6() { /* changed */ }".to_string();
        main_lines[6] = "fn modified_7() { /* changed */ }".to_string();
        main_lines.push("fn added_21() {}".to_string());
        main_lines.push("fn added_22() {}".to_string());
        main_lines.push("fn added_23() {}".to_string());
        main_lines.push("fn added_24() {}".to_string());
        std::fs::write(dir.path().join("src/main.rs"), main_lines.join("\n") + "\n").unwrap();

        // 4. Modify README.md: remove last 3 lines, add 5 new lines
        let mut readme_lines: Vec<String> = (1..=7).map(|i| format!("# Section {i}")).collect();
        readme_lines.push("# New Section A".to_string());
        readme_lines.push("# New Section B".to_string());
        readme_lines.push("# New Section C".to_string());
        readme_lines.push("# New Section D".to_string());
        readme_lines.push("# New Section E".to_string());
        std::fs::write(dir.path().join("README.md"), readme_lines.join("\n") + "\n").unwrap();

        // 5. Modify config.toml: change 2 of 5 lines
        std::fs::write(
            dir.path().join("config.toml"),
            "key1 = \"changed1\"\nkey2 = \"val2\"\nkey3 = \"changed3\"\nkey4 = \"val4\"\nkey5 = \"val5\"\n",
        )
        .unwrap();

        // Get jj diff --stat output (triggers snapshot internally)
        let jj_output = jj_cmd(dir.path(), &["diff", "--stat"]).await;
        let (cli_files, cli_added, cli_removed) = parse_diff_stat_summary(&jj_output);

        // Get our computed stats
        let config = Config {
            color: false,
            ..Default::default()
        };
        let status = query_jj_status(dir.path(), &config).await.unwrap();

        assert_eq!(
            (
                status.file_mad_count_working_tree,
                status.lines_added_working_tree,
                status.lines_removed_working_tree
            ),
            (cli_files, cli_added, cli_removed),
            "our stats ({}f, +{}, -{}) != jj diff --stat ({}f, +{}, -{})\njj output:\n{}",
            status.file_mad_count_working_tree,
            status.lines_added_working_tree,
            status.lines_removed_working_tree,
            cli_files,
            cli_added,
            cli_removed,
            jj_output,
        );
    }

    /// Scenario with interleaved insertions, deletions, and changes within a
    /// single large file. Verifies line-level diff accuracy.
    #[tokio::test]
    async fn test_diff_stats_match_jj_cli_single_file_complex() {
        let dir = create_jj_repo().await;

        // Create a 50-line file
        let initial: String = (1..=50).map(|i| format!("original line {i}\n")).collect();
        std::fs::write(dir.path().join("big.txt"), &initial).unwrap();

        jj_cmd(dir.path(), &["new"]).await;

        // Build modified version:
        // - Remove lines 5-8 (4 lines deleted)
        // - Change lines 15-17 (3 lines changed)
        // - Insert 6 new lines after line 30
        // - Remove lines 45-50 (6 lines deleted)
        let mut lines: Vec<String> = Vec::new();
        for i in 1..=50 {
            match i {
                5..=8 => continue, // deleted
                15 => lines.push("changed line 15".to_string()),
                16 => lines.push("changed line 16".to_string()),
                17 => lines.push("changed line 17".to_string()),
                30 => {
                    lines.push(format!("original line {i}"));
                    for j in 1..=6 {
                        lines.push(format!("inserted line {j}"));
                    }
                }
                45..=50 => continue, // deleted
                _ => lines.push(format!("original line {i}")),
            }
        }
        std::fs::write(dir.path().join("big.txt"), lines.join("\n") + "\n").unwrap();

        let jj_output = jj_cmd(dir.path(), &["diff", "--stat"]).await;
        let (cli_files, cli_added, cli_removed) = parse_diff_stat_summary(&jj_output);

        let config = Config {
            color: false,
            ..Default::default()
        };
        let status = query_jj_status(dir.path(), &config).await.unwrap();

        assert_eq!(
            (
                status.file_mad_count_working_tree,
                status.lines_added_working_tree,
                status.lines_removed_working_tree
            ),
            (cli_files, cli_added, cli_removed),
            "our stats ({}f, +{}, -{}) != jj diff --stat ({}f, +{}, -{})\njj output:\n{}",
            status.file_mad_count_working_tree,
            status.lines_added_working_tree,
            status.lines_removed_working_tree,
            cli_files,
            cli_added,
            cli_removed,
            jj_output,
        );
    }

    /// Scenario with files that share common prefixes/suffixes in their content,
    /// which can trip up diff algorithms. Also tests empty-to-content and
    /// content-to-empty transitions.
    #[tokio::test]
    async fn test_diff_stats_match_jj_cli_tricky_content() {
        let dir = create_jj_repo().await;

        // File that will go from content to empty
        std::fs::write(dir.path().join("shrink.txt"), "aaa\nbbb\nccc\nddd\neee\n").unwrap();

        // File with repeated/similar lines (harder for diff algorithms)
        let repetitive: String = (1..=20)
            .map(|i| {
                if i % 3 == 0 {
                    "repeated pattern\n".to_string()
                } else {
                    format!("unique line {i}\n")
                }
            })
            .collect();
        std::fs::write(dir.path().join("repetitive.txt"), &repetitive).unwrap();

        // File that will be completely rewritten
        let before_rewrite: String = (1..=10).map(|i| format!("before {i}\n")).collect();
        std::fs::write(dir.path().join("rewrite.txt"), &before_rewrite).unwrap();

        jj_cmd(dir.path(), &["new"]).await;

        // shrink.txt → empty content (but file still exists)
        std::fs::write(dir.path().join("shrink.txt"), "").unwrap();

        // repetitive.txt: shuffle some repeated lines, change unique ones
        let modified_rep: String = (1..=20)
            .map(|i| match i {
                3 => "different pattern\n".to_string(),
                6 => "another pattern\n".to_string(),
                7 => "changed unique 7\n".to_string(),
                13 => "changed unique 13\n".to_string(),
                _ if i % 3 == 0 => "repeated pattern\n".to_string(),
                _ => format!("unique line {i}\n"),
            })
            .collect();
        std::fs::write(dir.path().join("repetitive.txt"), &modified_rep).unwrap();

        // rewrite.txt: completely different content
        let after_rewrite: String = (1..=12).map(|i| format!("after {i}\n")).collect();
        std::fs::write(dir.path().join("rewrite.txt"), &after_rewrite).unwrap();

        // New file from nothing
        std::fs::write(dir.path().join("brand_new.txt"), "new1\nnew2\nnew3\n").unwrap();

        let jj_output = jj_cmd(dir.path(), &["diff", "--stat"]).await;
        let (cli_files, cli_added, cli_removed) = parse_diff_stat_summary(&jj_output);

        let config = Config {
            color: false,
            ..Default::default()
        };
        let status = query_jj_status(dir.path(), &config).await.unwrap();

        assert_eq!(
            (
                status.file_mad_count_working_tree,
                status.lines_added_working_tree,
                status.lines_removed_working_tree
            ),
            (cli_files, cli_added, cli_removed),
            "our stats ({}f, +{}, -{}) != jj diff --stat ({}f, +{}, -{})\njj output:\n{}",
            status.file_mad_count_working_tree,
            status.lines_added_working_tree,
            status.lines_removed_working_tree,
            cli_files,
            cli_added,
            cli_removed,
            jj_output,
        );
    }

    /// Test incremental diff: write a file after initial query (without snapshot)
    /// and verify the jj worker picks up the change via IncrementalUpdate.
    #[tokio::test]
    async fn test_incremental_diff_new_file() {
        let dir = create_jj_repo().await;
        let config = Config {
            color: false,
            ..Default::default()
        };

        // Initial full refresh — empty repo, no files changed
        let jj_worker = spawn_jj_worker();
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        jj_worker
            .send(JjWorkerRequest::FullRefresh {
                repo_path: dir.path().to_path_buf(),
                depth: config.bookmark_search_depth,
                reply: reply_tx,
            })
            .unwrap();
        let status = reply_rx.await.unwrap().unwrap();
        assert!(status.empty, "should be empty initially");
        assert_eq!(status.file_mad_count_working_tree, 0);

        // Write a file to working copy WITHOUT running jj (no snapshot)
        std::fs::write(dir.path().join("hello.txt"), "line1\nline2\nline3\n").unwrap();

        // Incremental update — should see the new file
        let abs_path = dir.path().canonicalize().unwrap().join("hello.txt");
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        jj_worker
            .send(JjWorkerRequest::IncrementalUpdate {
                repo_path: dir.path().to_path_buf(),
                changed_paths: vec![abs_path],
                reply: reply_tx,
            })
            .unwrap();
        let status = reply_rx.await.unwrap().unwrap();
        assert_eq!(
            status.file_mad_count_working_tree, 1,
            "should see 1 file changed"
        );
        assert_eq!(
            status.lines_added_working_tree, 3,
            "should see 3 lines added"
        );
        assert_eq!(status.lines_removed_working_tree, 0);
        assert!(!status.empty, "should not be empty after file write");
    }

    /// Test incremental diff: modify an existing snapshotted file and verify
    /// the overlay correctly replaces the base stats.
    #[tokio::test]
    async fn test_incremental_diff_modify_file() {
        let dir = create_jj_repo().await;
        // Create and snapshot a file
        std::fs::write(dir.path().join("data.txt"), "aaa\nbbb\nccc\n").unwrap();
        jj_cmd(dir.path(), &["status"]).await; // snapshot

        let config = Config {
            color: false,
            ..Default::default()
        };

        // Full refresh — sees the snapshotted diff
        let jj_worker = spawn_jj_worker();
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        jj_worker
            .send(JjWorkerRequest::FullRefresh {
                repo_path: dir.path().to_path_buf(),
                depth: config.bookmark_search_depth,
                reply: reply_tx,
            })
            .unwrap();
        let status = reply_rx.await.unwrap().unwrap();
        assert_eq!(status.file_mad_count_working_tree, 1);
        assert_eq!(status.lines_added_working_tree, 3);

        // Modify the file on disk without snapshot — add a line
        std::fs::write(dir.path().join("data.txt"), "aaa\nbbb\nccc\nddd\n").unwrap();

        let abs_path = dir.path().canonicalize().unwrap().join("data.txt");
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        jj_worker
            .send(JjWorkerRequest::IncrementalUpdate {
                repo_path: dir.path().to_path_buf(),
                changed_paths: vec![abs_path],
                reply: reply_tx,
            })
            .unwrap();
        let status = reply_rx.await.unwrap().unwrap();
        assert_eq!(status.file_mad_count_working_tree, 1);
        assert_eq!(
            status.lines_added_working_tree, 4,
            "should see 4 lines added (vs parent)"
        );
        assert_eq!(status.lines_removed_working_tree, 0);
    }

    /// Test incremental diff: delete a file that existed in parent.
    #[tokio::test]
    async fn test_incremental_diff_delete_file() {
        let dir = create_jj_repo().await;
        std::fs::write(dir.path().join("to_delete.txt"), "x\ny\nz\n").unwrap();
        jj_cmd(dir.path(), &["new"]).await; // commit the file

        let config = Config {
            color: false,
            ..Default::default()
        };

        // Full refresh — parent has to_delete.txt, current commit is empty
        let jj_worker = spawn_jj_worker();
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        jj_worker
            .send(JjWorkerRequest::FullRefresh {
                repo_path: dir.path().to_path_buf(),
                depth: config.bookmark_search_depth,
                reply: reply_tx,
            })
            .unwrap();
        let status = reply_rx.await.unwrap().unwrap();
        assert!(status.empty, "new commit should be empty");

        // Delete the file on disk without snapshot
        std::fs::remove_file(dir.path().join("to_delete.txt")).unwrap();

        let abs_path = dir.path().canonicalize().unwrap().join("to_delete.txt");
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        jj_worker
            .send(JjWorkerRequest::IncrementalUpdate {
                repo_path: dir.path().to_path_buf(),
                changed_paths: vec![abs_path],
                reply: reply_tx,
            })
            .unwrap();
        let status = reply_rx.await.unwrap().unwrap();
        assert_eq!(
            status.file_mad_count_working_tree, 1,
            "should see 1 deleted file"
        );
        assert_eq!(
            status.lines_removed_working_tree, 3,
            "should see 3 lines removed"
        );
        assert_eq!(status.lines_added_working_tree, 0);
    }

    // --- Pure unit tests for overlay aggregation (no jj repo needed) ---

    /// Helper to build DiffCounts with just (file_mad_count, lines_added, lines_removed).
    /// file_mad_count are all counted as modified.
    fn dcounts(file_mad_count: u32, lines_added: u32, lines_removed: u32) -> DiffCounts {
        DiffCounts {
            file_mad_count,
            lines_added,
            lines_removed,
            files_modified: file_mad_count,
            ..Default::default()
        }
    }

    fn fstats(added: u32, removed: u32) -> FileDiffStats {
        FileDiffStats {
            lines_added: added,
            lines_removed: removed,
            kind: FileChangeKind::Modified,
            ..Default::default()
        }
    }

    #[test]
    fn test_aggregate_empty() {
        let base = HashMap::new();
        let overlay = HashMap::new();
        assert_eq!(aggregate_overlay_stats(&base, &overlay), dcounts(0, 0, 0));
    }

    #[test]
    fn test_aggregate_base_only() {
        let base = HashMap::from([
            ("a.rs".into(), fstats(10, 3)),
            ("b.rs".into(), fstats(5, 0)),
        ]);
        let overlay = HashMap::new();
        assert_eq!(aggregate_overlay_stats(&base, &overlay), dcounts(2, 15, 3));
    }

    #[test]
    fn test_aggregate_overlay_replaces_base() {
        let base = HashMap::from([("a.rs".into(), fstats(10, 3))]);
        // Overlay says a.rs now has 20 added, 1 removed (e.g., user added more lines)
        let overlay = HashMap::from([("a.rs".into(), Some(fstats(20, 1)))]);
        assert_eq!(aggregate_overlay_stats(&base, &overlay), dcounts(1, 20, 1));
    }

    #[test]
    fn test_aggregate_overlay_reverts_file() {
        // Base shows a.rs changed, but overlay says it now matches parent
        let base = HashMap::from([("a.rs".into(), fstats(10, 3))]);
        let overlay = HashMap::from([("a.rs".into(), None)]);
        assert_eq!(aggregate_overlay_stats(&base, &overlay), dcounts(0, 0, 0));
    }

    #[test]
    fn test_aggregate_overlay_new_file() {
        // Base has no files, overlay adds a new file
        let base = HashMap::new();
        let overlay = HashMap::from([("new.txt".into(), Some(fstats(5, 0)))]);
        assert_eq!(aggregate_overlay_stats(&base, &overlay), dcounts(1, 5, 0));
    }

    #[test]
    fn test_aggregate_mixed_base_and_overlay() {
        let base = HashMap::from([
            ("unchanged.rs".into(), fstats(10, 2)), // no overlay → kept
            ("modified.rs".into(), fstats(5, 1)),   // overlay replaces
            ("reverted.rs".into(), fstats(8, 3)),   // overlay reverts to parent
        ]);
        let overlay = HashMap::from([
            ("modified.rs".into(), Some(fstats(7, 0))),
            ("reverted.rs".into(), None),
            ("brand_new.rs".into(), Some(fstats(20, 0))),
        ]);
        // unchanged.rs: +10 -2 (from base)
        // modified.rs:  +7  -0 (from overlay)
        // reverted.rs:  excluded
        // brand_new.rs: +20 -0 (from overlay, not in base)
        assert_eq!(aggregate_overlay_stats(&base, &overlay), dcounts(3, 37, 2));
    }

    #[test]
    fn test_aggregate_overlay_zeros_out_file() {
        // Overlay replaces base with zero line stats (file still differs, e.g.
        // binary or exec-bit change). The file counts, matching `diff --stat`
        // ("1 file changed, 0 insertions(+), 0 deletions(-)"). A file that no
        // longer differs must be represented by `None`, not zero stats.
        let base = HashMap::from([("a.rs".into(), fstats(10, 3))]);
        let overlay = HashMap::from([("a.rs".into(), Some(fstats(0, 0)))]);
        assert_eq!(aggregate_overlay_stats(&base, &overlay), dcounts(1, 0, 0));
    }

    #[test]
    fn test_aggregate_overlay_new_file_with_no_stats() {
        // New file in overlay but with None (deleted before it was ever in base)
        let base = HashMap::new();
        let overlay = HashMap::from([("phantom.txt".into(), None)]);
        assert_eq!(aggregate_overlay_stats(&base, &overlay), dcounts(0, 0, 0));
    }

    #[test]
    fn test_aggregate_multiple_overlays_accumulate() {
        // Simulate multiple file changes in the overlay
        let base = HashMap::from([("a.rs".into(), fstats(5, 1))]);
        let overlay = HashMap::from([
            ("a.rs".into(), Some(fstats(8, 2))),
            ("b.rs".into(), Some(fstats(3, 0))),
            ("c.rs".into(), Some(fstats(0, 4))),
            ("d.rs".into(), None), // not in base, reverted
        ]);
        // a.rs: overlay +8 -2
        // b.rs: overlay +3 -0
        // c.rs: overlay +0 -4
        // d.rs: None, not in base → excluded
        assert_eq!(aggregate_overlay_stats(&base, &overlay), dcounts(3, 11, 6));
    }

    #[test]
    fn test_aggregate_base_zero_line_stats_counted_as_file() {
        // A base entry always represents a real change; zero line stats
        // (empty/binary file, exec-bit change) still count the file,
        // matching `diff --stat`.
        let base = HashMap::from([("empty_diff.rs".into(), fstats(0, 0))]);
        let overlay = HashMap::new();
        assert_eq!(aggregate_overlay_stats(&base, &overlay), dcounts(1, 0, 0));
    }

    #[test]
    fn test_aggregate_all_reverted() {
        let base = HashMap::from([
            ("a.rs".into(), fstats(10, 2)),
            ("b.rs".into(), fstats(5, 1)),
            ("c.rs".into(), fstats(3, 0)),
        ]);
        let overlay = HashMap::from([
            ("a.rs".into(), None),
            ("b.rs".into(), None),
            ("c.rs".into(), None),
        ]);
        assert_eq!(aggregate_overlay_stats(&base, &overlay), dcounts(0, 0, 0));
    }

    #[test]
    fn test_aggregate_overlay_only_new_files() {
        let base = HashMap::new();
        let overlay = HashMap::from([
            ("x.rs".into(), Some(fstats(1, 0))),
            ("y.rs".into(), Some(fstats(0, 1))),
            ("z.rs".into(), Some(fstats(10, 5))),
        ]);
        assert_eq!(aggregate_overlay_stats(&base, &overlay), dcounts(3, 11, 6));
    }

    // --- Sequential overlay accumulation tests ---
    // These simulate the pattern of events arriving over time: the overlay
    // is mutated between aggregate_overlay_stats calls, just as the jj worker
    // processes IncrementalUpdate requests sequentially.

    /// Helper: apply a single file event to an overlay (mirrors jj_worker_loop logic).
    /// `diff` is `Some(stats)` if the file differs from parent, `None` if it matches.
    fn apply_event(
        overlay: &mut HashMap<String, Option<FileDiffStats>>,
        path: &str,
        diff: Option<FileDiffStats>,
    ) {
        overlay.insert(path.to_string(), diff);
    }

    #[test]
    fn test_sequential_events_accumulate() {
        // Base: one file snapshotted
        let base = HashMap::from([("existing.rs".into(), fstats(5, 1))]);
        let mut overlay = HashMap::new();

        // Event 1: new file created
        apply_event(&mut overlay, "new1.txt", Some(fstats(3, 0)));
        assert_eq!(aggregate_overlay_stats(&base, &overlay), dcounts(2, 8, 1));

        // Event 2: another new file created (while event 1 was being processed)
        apply_event(&mut overlay, "new2.txt", Some(fstats(7, 0)));
        assert_eq!(aggregate_overlay_stats(&base, &overlay), dcounts(3, 15, 1));

        // Event 3: yet another file
        apply_event(&mut overlay, "new3.txt", Some(fstats(1, 0)));
        assert_eq!(aggregate_overlay_stats(&base, &overlay), dcounts(4, 16, 1));
    }

    #[test]
    fn test_sequential_same_file_modified_multiple_times() {
        // User saves a file repeatedly — each event replaces the previous overlay entry
        let base = HashMap::new();
        let mut overlay = HashMap::new();

        // Save 1: 5 lines added
        apply_event(&mut overlay, "main.rs", Some(fstats(5, 0)));
        assert_eq!(aggregate_overlay_stats(&base, &overlay), dcounts(1, 5, 0));

        // Save 2: user adds more → 8 lines total (not cumulative, replaces)
        apply_event(&mut overlay, "main.rs", Some(fstats(8, 0)));
        assert_eq!(aggregate_overlay_stats(&base, &overlay), dcounts(1, 8, 0));

        // Save 3: user deletes some lines → 6 added, 2 removed
        apply_event(&mut overlay, "main.rs", Some(fstats(6, 2)));
        assert_eq!(aggregate_overlay_stats(&base, &overlay), dcounts(1, 6, 2));

        // Save 4: user reverts to match parent exactly
        apply_event(&mut overlay, "main.rs", None);
        assert_eq!(aggregate_overlay_stats(&base, &overlay), dcounts(0, 0, 0));
    }

    #[test]
    fn test_sequential_create_modify_delete() {
        // File lifecycle: created → modified → deleted
        let base = HashMap::new();
        let mut overlay = HashMap::new();

        // Create file
        apply_event(&mut overlay, "temp.txt", Some(fstats(10, 0)));
        assert_eq!(aggregate_overlay_stats(&base, &overlay), dcounts(1, 10, 0));

        // Modify it
        apply_event(&mut overlay, "temp.txt", Some(fstats(12, 0)));
        assert_eq!(aggregate_overlay_stats(&base, &overlay), dcounts(1, 12, 0));

        // Delete it — since it's not in base/parent, None means it's gone entirely
        apply_event(&mut overlay, "temp.txt", None);
        assert_eq!(aggregate_overlay_stats(&base, &overlay), dcounts(0, 0, 0));
    }

    #[test]
    fn test_sequential_interleaved_files() {
        // Events for different files interleaved: A, B, A, C, B
        let base = HashMap::from([("a.rs".into(), fstats(3, 1))]);
        let mut overlay = HashMap::new();

        // Event: a.rs modified on disk (replaces base)
        apply_event(&mut overlay, "a.rs", Some(fstats(5, 2)));
        assert_eq!(aggregate_overlay_stats(&base, &overlay), dcounts(1, 5, 2));

        // Event: b.rs created
        apply_event(&mut overlay, "b.rs", Some(fstats(4, 0)));
        assert_eq!(aggregate_overlay_stats(&base, &overlay), dcounts(2, 9, 2));

        // Event: a.rs modified again
        apply_event(&mut overlay, "a.rs", Some(fstats(6, 3)));
        assert_eq!(aggregate_overlay_stats(&base, &overlay), dcounts(2, 10, 3));

        // Event: c.rs created
        apply_event(&mut overlay, "c.rs", Some(fstats(1, 0)));
        assert_eq!(aggregate_overlay_stats(&base, &overlay), dcounts(3, 11, 3));

        // Event: b.rs deleted (was only in overlay, not parent)
        apply_event(&mut overlay, "b.rs", None);
        assert_eq!(aggregate_overlay_stats(&base, &overlay), dcounts(2, 7, 3));
    }

    #[test]
    fn test_sequential_revert_snapshotted_then_re_modify() {
        // File exists in base (was snapshotted), user reverts on disk, then modifies again
        let base = HashMap::from([("config.toml".into(), fstats(2, 1))]);
        let mut overlay = HashMap::new();

        // User reverts file to match parent
        apply_event(&mut overlay, "config.toml", None);
        assert_eq!(aggregate_overlay_stats(&base, &overlay), dcounts(0, 0, 0));

        // User makes a different change
        apply_event(&mut overlay, "config.toml", Some(fstats(10, 5)));
        assert_eq!(aggregate_overlay_stats(&base, &overlay), dcounts(1, 10, 5));
    }

    #[test]
    fn test_sequential_full_refresh_clears_overlay() {
        // Simulate: events accumulate, then a full refresh replaces base and clears overlay
        let mut base = HashMap::from([("old.rs".into(), fstats(3, 0))]);
        let mut overlay = HashMap::new();

        // Accumulate overlay events
        apply_event(&mut overlay, "new.txt", Some(fstats(5, 0)));
        apply_event(&mut overlay, "old.rs", Some(fstats(10, 2)));
        assert_eq!(aggregate_overlay_stats(&base, &overlay), dcounts(2, 15, 2));

        // Full refresh: new base, overlay cleared (simulates what jj_worker_loop does)
        base = HashMap::from([
            ("old.rs".into(), fstats(10, 2)), // now snapshotted with the overlay values
            ("new.txt".into(), fstats(5, 0)), // also snapshotted
        ]);
        overlay.clear();
        assert_eq!(aggregate_overlay_stats(&base, &overlay), dcounts(2, 15, 2));

        // New events on the fresh base
        apply_event(&mut overlay, "old.rs", Some(fstats(11, 2)));
        assert_eq!(aggregate_overlay_stats(&base, &overlay), dcounts(2, 16, 2));
    }

    #[test]
    fn test_sequential_many_rapid_events() {
        // Simulate a build tool writing many files quickly
        let base = HashMap::new();
        let mut overlay = HashMap::new();

        for i in 0..50 {
            let path = format!("src/gen_{i}.rs");
            apply_event(&mut overlay, &path, Some(fstats(10, 0)));
        }
        assert_eq!(
            aggregate_overlay_stats(&base, &overlay),
            dcounts(50, 500, 0)
        );

        // Then all get deleted (e.g., clean build)
        for i in 0..50 {
            let path = format!("src/gen_{i}.rs");
            apply_event(&mut overlay, &path, None);
        }
        assert_eq!(aggregate_overlay_stats(&base, &overlay), dcounts(0, 0, 0));
    }

    #[test]
    fn test_sequential_base_file_deleted_on_disk() {
        // Base has files from snapshot; user deletes one of them on disk
        let base = HashMap::from([
            ("keep.rs".into(), fstats(20, 5)),
            ("delete_me.rs".into(), fstats(10, 3)),
        ]);
        let mut overlay = HashMap::new();

        // User deletes delete_me.rs — parent had content, disk has nothing.
        // diff_single_file would return Some(fstats(0, <parent_lines>))
        // since all parent lines become removals.
        apply_event(&mut overlay, "delete_me.rs", Some(fstats(0, 15)));
        // keep.rs: base +20 -5
        // delete_me.rs: overlay +0 -15 (replaces base +10 -3)
        assert_eq!(aggregate_overlay_stats(&base, &overlay), dcounts(2, 20, 20));
    }

    #[test]
    fn test_abs_to_repo_relative_basic() {
        let root = Path::new("/home/user/repo");
        assert_eq!(
            abs_to_repo_relative(root, Path::new("/home/user/repo/src/main.rs")),
            Some("src/main.rs".to_string())
        );
    }

    #[test]
    fn test_abs_to_repo_relative_root_file() {
        let root = Path::new("/repo");
        assert_eq!(
            abs_to_repo_relative(root, Path::new("/repo/file.txt")),
            Some("file.txt".to_string())
        );
    }

    #[test]
    fn test_abs_to_repo_relative_outside() {
        let root = Path::new("/home/user/repo");
        assert_eq!(
            abs_to_repo_relative(root, Path::new("/home/other/file.txt")),
            None
        );
    }

    /// ValidateAndRefresh after `jj describe`: parent tree unchanged, so overlay
    /// should be preserved and only metadata (description) should update.
    #[tokio::test]
    async fn test_validate_refresh_jj_describe_preserves_overlay() {
        let dir = create_jj_repo().await;
        let config = Config {
            color: false,
            ..Default::default()
        };

        // Create a file and snapshot it
        std::fs::write(dir.path().join("file.txt"), "line1\nline2\n").unwrap();
        jj_cmd(dir.path(), &["status"]).await;

        // Full refresh — establishes baseline with 1 file, 2 lines added
        let jj_worker = spawn_jj_worker();
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        jj_worker
            .send(JjWorkerRequest::FullRefresh {
                repo_path: dir.path().to_path_buf(),
                depth: config.bookmark_search_depth,
                reply: reply_tx,
            })
            .unwrap();
        let status = reply_rx.await.unwrap().unwrap();
        assert_eq!(status.file_mad_count_working_tree, 1);
        assert_eq!(status.lines_added_working_tree, 2);
        let original_description = status.description.clone();

        // `jj describe` — changes metadata but not parent tree
        jj_cmd(dir.path(), &["describe", "-m", "new description"]).await;

        // ValidateAndRefresh — should detect unchanged parent tree, do metadata-only
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        jj_worker
            .send(JjWorkerRequest::ValidateAndRefresh {
                repo_path: dir.path().to_path_buf(),
                changed_paths: vec![],
                depth: config.bookmark_search_depth,
                reply: reply_tx,
            })
            .unwrap();
        let status = reply_rx.await.unwrap().unwrap();

        // Metadata should be updated
        assert_eq!(status.description, "new description");
        assert_ne!(status.description, original_description);

        // Overlay/diffs should be preserved — same file stats as before
        assert_eq!(
            status.file_mad_count_working_tree, 1,
            "overlay should be preserved after describe"
        );
        assert_eq!(
            status.lines_added_working_tree, 2,
            "line stats should be preserved after describe"
        );
    }

    /// ValidateAndRefresh after `jj bookmark create`: parent tree unchanged,
    /// so overlay should be preserved and bookmark metadata should update.
    #[tokio::test]
    async fn test_validate_refresh_jj_bookmark_preserves_overlay() {
        let dir = create_jj_repo().await;
        let config = Config {
            color: false,
            ..Default::default()
        };

        // Create a file and snapshot it
        std::fs::write(dir.path().join("file.txt"), "content\n").unwrap();
        jj_cmd(dir.path(), &["status"]).await;

        // Full refresh
        let jj_worker = spawn_jj_worker();
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        jj_worker
            .send(JjWorkerRequest::FullRefresh {
                repo_path: dir.path().to_path_buf(),
                depth: config.bookmark_search_depth,
                reply: reply_tx,
            })
            .unwrap();
        let status = reply_rx.await.unwrap().unwrap();
        assert_eq!(status.file_mad_count_working_tree, 1);
        assert!(status.bookmarks.is_empty());

        // Create a bookmark
        jj_cmd(dir.path(), &["bookmark", "create", "my-branch"]).await;

        // ValidateAndRefresh
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        jj_worker
            .send(JjWorkerRequest::ValidateAndRefresh {
                repo_path: dir.path().to_path_buf(),
                changed_paths: vec![],
                depth: config.bookmark_search_depth,
                reply: reply_tx,
            })
            .unwrap();
        let status = reply_rx.await.unwrap().unwrap();

        // Bookmark metadata updated
        assert!(
            !status.bookmarks.is_empty(),
            "bookmark should appear after create"
        );

        // Diffs preserved
        assert_eq!(
            status.file_mad_count_working_tree, 1,
            "overlay should be preserved after bookmark create"
        );
    }

    /// ValidateAndRefresh after `jj new`: parent tree changes (new empty commit),
    /// so a full refresh should be triggered.
    #[tokio::test]
    async fn test_validate_refresh_jj_new_triggers_full_refresh() {
        let dir = create_jj_repo().await;
        let config = Config {
            color: false,
            ..Default::default()
        };

        // Create a file and snapshot it
        std::fs::write(dir.path().join("file.txt"), "line1\nline2\n").unwrap();
        jj_cmd(dir.path(), &["status"]).await;

        // Full refresh — 1 file, 2 lines added
        let jj_worker = spawn_jj_worker();
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        jj_worker
            .send(JjWorkerRequest::FullRefresh {
                repo_path: dir.path().to_path_buf(),
                depth: config.bookmark_search_depth,
                reply: reply_tx,
            })
            .unwrap();
        let status = reply_rx.await.unwrap().unwrap();
        assert_eq!(status.file_mad_count_working_tree, 1);
        assert_eq!(status.lines_added_working_tree, 2);
        let old_change_id = status.change_id.clone();

        // `jj new` — creates a new empty change on top, parent tree now includes file.txt
        jj_cmd(dir.path(), &["new"]).await;

        // ValidateAndRefresh — parent tree changed, should trigger full refresh
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        jj_worker
            .send(JjWorkerRequest::ValidateAndRefresh {
                repo_path: dir.path().to_path_buf(),
                changed_paths: vec![],
                depth: config.bookmark_search_depth,
                reply: reply_tx,
            })
            .unwrap();
        let status = reply_rx.await.unwrap().unwrap();

        // After `jj new`, file.txt is in the parent tree so working tree diff is 0
        assert_eq!(
            status.file_mad_count_working_tree, 0,
            "file.txt is now in parent, so diff should be empty"
        );
        assert_ne!(
            status.change_id, old_change_id,
            "should have a new change ID after jj new"
        );
        assert!(status.empty, "new empty change should be empty");
    }

    /// ValidateAndRefresh with incremental WC diffs: after `jj describe`,
    /// also apply incremental diffs for changed_paths.
    #[tokio::test]
    async fn test_validate_refresh_jj_describe_with_wc_diffs() {
        let dir = create_jj_repo().await;
        let config = Config {
            color: false,
            ..Default::default()
        };

        // Create and snapshot a file
        std::fs::write(dir.path().join("a.txt"), "original\n").unwrap();
        jj_cmd(dir.path(), &["status"]).await;

        // Full refresh
        let jj_worker = spawn_jj_worker();
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        jj_worker
            .send(JjWorkerRequest::FullRefresh {
                repo_path: dir.path().to_path_buf(),
                depth: config.bookmark_search_depth,
                reply: reply_tx,
            })
            .unwrap();
        let status = reply_rx.await.unwrap().unwrap();
        assert_eq!(status.lines_added_working_tree, 1);

        // Modify a file on disk AND run `jj describe` (simulates mixed event)
        std::fs::write(dir.path().join("a.txt"), "original\nnew line\n").unwrap();
        jj_cmd(dir.path(), &["describe", "-m", "updated"]).await;

        // ValidateAndRefresh with changed_paths
        let abs_path = dir.path().canonicalize().unwrap().join("a.txt");
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        jj_worker
            .send(JjWorkerRequest::ValidateAndRefresh {
                repo_path: dir.path().to_path_buf(),
                changed_paths: vec![abs_path],
                depth: config.bookmark_search_depth,
                reply: reply_tx,
            })
            .unwrap();
        let status = reply_rx.await.unwrap().unwrap();

        // Description updated
        assert_eq!(status.description, "updated");

        // Incremental diff applied — now 2 lines added
        assert_eq!(
            status.lines_added_working_tree, 2,
            "should see updated line count from incremental diff"
        );
    }

    /// Regression test: `jj bookmark set -r PAST_CHANGE` should not affect
    /// the working copy diff stats. The bookmark points to a past revision,
    /// so the parent tree of the working copy is unchanged.
    #[tokio::test]
    async fn test_validate_refresh_jj_bookmark_set_past_revision() {
        let dir = create_jj_repo().await;
        let config = Config {
            color: false,
            ..Default::default()
        };

        // Build some history: create file, commit, create another file
        std::fs::write(dir.path().join("a.txt"), "aaa\n").unwrap();
        jj_cmd(dir.path(), &["commit", "-m", "first"]).await;
        std::fs::write(dir.path().join("b.txt"), "bbb\n").unwrap();
        jj_cmd(dir.path(), &["commit", "-m", "second"]).await;

        // Working copy: add a new file
        std::fs::write(dir.path().join("c.txt"), "ccc\n").unwrap();
        jj_cmd(dir.path(), &["status"]).await; // snapshot

        // Full refresh — should see 1 file, 1 line added (only c.txt)
        let jj_worker = spawn_jj_worker();
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        jj_worker
            .send(JjWorkerRequest::FullRefresh {
                repo_path: dir.path().to_path_buf(),
                depth: config.bookmark_search_depth,
                reply: reply_tx,
            })
            .unwrap();
        let status = reply_rx.await.unwrap().unwrap();
        let original_file_count = status.file_mad_count_working_tree;
        let original_lines_added = status.lines_added_working_tree;
        assert_eq!(original_file_count, 1, "only c.txt should be changed");
        assert_eq!(original_lines_added, 1, "only 1 line added");

        // Set a bookmark to the past "first" revision (@ is 2 commits ahead)
        jj_cmd(dir.path(), &["bookmark", "set", "-r", "@--", "test-bm"]).await;

        // ValidateAndRefresh — parent tree should be unchanged
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        jj_worker
            .send(JjWorkerRequest::ValidateAndRefresh {
                repo_path: dir.path().to_path_buf(),
                changed_paths: vec![],
                depth: config.bookmark_search_depth,
                reply: reply_tx,
            })
            .unwrap();
        let status = reply_rx.await.unwrap().unwrap();

        // Bookmark should appear in metadata
        assert!(
            status.bookmarks.iter().any(|b| b.name == "test-bm"),
            "bookmark should appear after set"
        );

        // Diff stats must NOT change — the bookmark is on a past revision,
        // not the working copy's parent
        assert_eq!(
            status.file_mad_count_working_tree, original_file_count,
            "diff stats should be unchanged after bookmark set to past revision"
        );
        assert_eq!(
            status.lines_added_working_tree, original_lines_added,
            "line stats should be unchanged after bookmark set to past revision"
        );
    }

    /// Regression test: full refresh after `jj bookmark set -r PAST` should
    /// still show correct diff stats (only WC changes, not the entire history).
    #[tokio::test]
    async fn test_full_refresh_after_jj_bookmark_set_past_revision() {
        let dir = create_jj_repo().await;
        let config = Config {
            color: false,
            ..Default::default()
        };

        // Build history
        std::fs::write(dir.path().join("a.txt"), "aaa\n").unwrap();
        jj_cmd(dir.path(), &["commit", "-m", "first"]).await;
        std::fs::write(dir.path().join("b.txt"), "bbb\n").unwrap();
        jj_cmd(dir.path(), &["commit", "-m", "second"]).await;

        // Working copy: add a new file
        std::fs::write(dir.path().join("c.txt"), "ccc\n").unwrap();
        jj_cmd(dir.path(), &["status"]).await; // snapshot

        // Full refresh before bookmark
        let status_before = query_jj_status(dir.path(), &config).await.unwrap();
        let files_before = status_before.file_mad_count_working_tree;
        let lines_before = status_before.lines_added_working_tree;

        // Set a bookmark to a past revision
        jj_cmd(dir.path(), &["bookmark", "set", "-r", "@--", "test-bm"]).await;

        // Full refresh after bookmark
        let status_after = query_jj_status(dir.path(), &config).await.unwrap();

        assert_eq!(
            status_after.file_mad_count_working_tree, files_before,
            "full refresh after bookmark set should show same file count"
        );
        assert_eq!(
            status_after.lines_added_working_tree, lines_before,
            "full refresh after bookmark set should show same line count"
        );
    }

    // ==== Regression tests for historical drift bugs ====

    /// `jj abandon` keeps the same parent tree but rewrites the WC commit.
    /// Even with every working-copy event dropped (FSEvents overflow /
    /// dir-level coalescing), ValidateAndRefresh must detect the commit tree
    /// change and rebuild base stats from the store.
    #[tokio::test]
    async fn repro_stale_base_after_abandon_without_events() {
        let dir = create_jj_repo().await;
        let config = Config {
            color: false,
            ..Default::default()
        };
        std::fs::write(dir.path().join("file.txt"), "line1\nline2\n").unwrap();
        jj_cmd(dir.path(), &["status"]).await;

        let jj_worker = spawn_jj_worker();
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        jj_worker
            .send(JjWorkerRequest::FullRefresh {
                repo_path: dir.path().to_path_buf(),
                depth: config.bookmark_search_depth,
                reply: reply_tx,
            })
            .unwrap();
        let status = reply_rx.await.unwrap().unwrap();
        assert_eq!(status.file_mad_count_working_tree, 1);
        assert_eq!(status.lines_added_working_tree, 2);

        // abandon @: new empty @ on the SAME parent; file.txt removed from disk
        jj_cmd(dir.path(), &["abandon"]).await;
        assert!(
            !dir.path().join("file.txt").exists(),
            "abandon should remove the file"
        );

        // Simulate the op_heads event arriving with the working-copy events dropped
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        jj_worker
            .send(JjWorkerRequest::ValidateAndRefresh {
                repo_path: dir.path().to_path_buf(),
                changed_paths: vec![],
                depth: config.bookmark_search_depth,
                reply: reply_tx,
            })
            .unwrap();
        let status = reply_rx.await.unwrap().unwrap();
        assert_eq!(
            (
                status.file_mad_count_working_tree,
                status.lines_added_working_tree
            ),
            (0, 0),
            "after abandon the WC is empty; base stats must be rebuilt from the store"
        );
    }

    /// An added empty file counts as a changed file in `jj diff --stat`
    /// ("1 file changed, 0 insertions"), and so must we.
    #[tokio::test]
    async fn repro_empty_added_file_not_counted() {
        let dir = create_jj_repo().await;
        let config = Config {
            color: false,
            ..Default::default()
        };
        std::fs::write(dir.path().join("empty.txt"), "").unwrap();
        let jj_output = jj_cmd(dir.path(), &["diff", "--stat"]).await;
        let (cli_files, cli_added, cli_removed) = parse_diff_stat_summary(&jj_output);
        let status = query_jj_status(dir.path(), &config).await.unwrap();
        assert_eq!(
            (
                status.file_mad_count_working_tree,
                status.lines_added_working_tree,
                status.lines_removed_working_tree
            ),
            (cli_files, cli_added, cli_removed),
            "jj CLI says: {jj_output}"
        );
    }

    /// File conflicted in the parent tree: the incremental single-file diff
    /// must materialize the conflicted parent value (conflict-marker text,
    /// as jj does) rather than treating it as absent — otherwise touching
    /// the file counts its entire contents as added.
    #[tokio::test]
    async fn repro_conflicted_parent_counts_whole_file_as_added() {
        let dir = create_jj_repo().await;
        let config = Config {
            color: false,
            ..Default::default()
        };
        // Build a conflict: two siblings editing the same line, then merge
        std::fs::write(dir.path().join("f.txt"), "base\n").unwrap();
        jj_cmd(dir.path(), &["commit", "-m", "base"]).await;
        jj_cmd(dir.path(), &["bookmark", "create", "base-bm", "-r", "@-"]).await;
        std::fs::write(dir.path().join("f.txt"), "side-a\n").unwrap();
        jj_cmd(dir.path(), &["commit", "-m", "a"]).await;
        jj_cmd(dir.path(), &["bookmark", "create", "side-a-bm", "-r", "@-"]).await;
        jj_cmd(dir.path(), &["new", "base-bm"]).await;
        std::fs::write(dir.path().join("f.txt"), "side-b\n").unwrap();
        jj_cmd(dir.path(), &["commit", "-m", "b"]).await;
        jj_cmd(dir.path(), &["bookmark", "create", "side-b-bm", "-r", "@-"]).await;
        // merge the two sides — @ tree has a conflict in f.txt
        jj_cmd(dir.path(), &["new", "side-a-bm", "side-b-bm"]).await;

        let jj_worker = spawn_jj_worker();
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        jj_worker
            .send(JjWorkerRequest::FullRefresh {
                repo_path: dir.path().to_path_buf(),
                depth: config.bookmark_search_depth,
                reply: reply_tx,
            })
            .unwrap();
        let full = reply_rx.await.unwrap().unwrap();
        eprintln!(
            "full refresh: files={} +{} -{}",
            full.file_mad_count_working_tree,
            full.lines_added_working_tree,
            full.lines_removed_working_tree
        );

        // Simulate a watcher event on the conflicted (marker-materialized) file
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        jj_worker
            .send(JjWorkerRequest::IncrementalUpdate {
                repo_path: dir.path().to_path_buf(),
                changed_paths: vec![dir.path().join("f.txt")],
                reply: reply_tx,
            })
            .unwrap();
        let incr = reply_rx.await.unwrap().unwrap();
        let disk_lines = count_lines(&std::fs::read(dir.path().join("f.txt")).unwrap());
        eprintln!(
            "incremental: files={} +{} -{} (disk file has {} lines)",
            incr.file_mad_count_working_tree,
            incr.lines_added_working_tree,
            incr.lines_removed_working_tree,
            disk_lines
        );
        assert_eq!(
            (
                incr.file_mad_count_working_tree,
                incr.lines_added_working_tree
            ),
            (
                full.file_mad_count_working_tree,
                full.lines_added_working_tree
            ),
            "touching a conflicted file should not change the diff stats"
        );
    }

    /// `ui.conflict-marker-style` from the repo config must be used when
    /// materializing conflicted parent values: with `git` style configured,
    /// the file jj writes to disk uses git markers, and touching it must
    /// still diff as unchanged. (With a hardcoded style, the marker text
    /// mismatch reports phantom modified lines.)
    #[tokio::test]
    async fn test_conflict_marker_style_config_respected() {
        let dir = create_jj_repo().await;
        let _config_cleanup = crate::test_util::RepoConfigCleanup::new(dir.path());
        let config = Config {
            color: false,
            ..Default::default()
        };
        // Configure BEFORE the merge is created so jj materializes the
        // on-disk conflict with git-style markers. Repo-level config also
        // keeps this test independent of the host user's jj config.
        jj_cmd(
            dir.path(),
            &["config", "set", "--repo", "ui.conflict-marker-style", "git"],
        )
        .await;
        assert_eq!(
            conflict_marker_style_for_repo(dir.path()),
            ConflictMarkerStyle::Git,
            "resolver should pick up the repo-level config"
        );

        // Build a conflict: two siblings editing the same line, then merge
        std::fs::write(dir.path().join("f.txt"), "base\n").unwrap();
        jj_cmd(dir.path(), &["commit", "-m", "base"]).await;
        jj_cmd(dir.path(), &["bookmark", "create", "base-bm", "-r", "@-"]).await;
        std::fs::write(dir.path().join("f.txt"), "side-a\n").unwrap();
        jj_cmd(dir.path(), &["commit", "-m", "a"]).await;
        jj_cmd(dir.path(), &["bookmark", "create", "side-a-bm", "-r", "@-"]).await;
        jj_cmd(dir.path(), &["new", "base-bm"]).await;
        std::fs::write(dir.path().join("f.txt"), "side-b\n").unwrap();
        jj_cmd(dir.path(), &["commit", "-m", "b"]).await;
        jj_cmd(dir.path(), &["bookmark", "create", "side-b-bm", "-r", "@-"]).await;
        jj_cmd(dir.path(), &["new", "side-a-bm", "side-b-bm"]).await;

        let disk = std::fs::read_to_string(dir.path().join("f.txt")).unwrap();
        assert!(
            disk.contains("<<<<<<<") && disk.contains("=======") && !disk.contains("%%%%%%%"),
            "on-disk conflict should use git-style markers, got:\n{disk}"
        );

        let jj_worker = spawn_jj_worker();
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        jj_worker
            .send(JjWorkerRequest::FullRefresh {
                repo_path: dir.path().to_path_buf(),
                depth: config.bookmark_search_depth,
                reply: reply_tx,
            })
            .unwrap();
        let full = reply_rx.await.unwrap().unwrap();

        // Touch the conflicted file (watcher event, content unchanged)
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        jj_worker
            .send(JjWorkerRequest::IncrementalUpdate {
                repo_path: dir.path().to_path_buf(),
                changed_paths: vec![dir.path().join("f.txt")],
                reply: reply_tx,
            })
            .unwrap();
        let incr = reply_rx.await.unwrap().unwrap();
        assert_eq!(
            (
                incr.file_mad_count_working_tree,
                incr.lines_added_working_tree,
                incr.lines_removed_working_tree
            ),
            (
                full.file_mad_count_working_tree,
                full.lines_added_working_tree,
                full.lines_removed_working_tree
            ),
            "touching a git-marker conflicted file must not change diff stats"
        );
    }

    /// The resolver falls back to jj's default (Diff) with no config, and
    /// ignores invalid values.
    #[test]
    fn test_conflict_marker_style_default_and_invalid() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join(".jj/repo")).unwrap();
        assert_eq!(
            conflict_marker_style_for_repo(dir.path()),
            ConflictMarkerStyle::Diff
        );

        std::fs::write(
            dir.path().join(".jj/repo/config.toml"),
            "[ui]\nconflict-marker-style = \"snapshot\"\n",
        )
        .unwrap();
        assert_eq!(
            conflict_marker_style_for_repo(dir.path()),
            ConflictMarkerStyle::Snapshot
        );

        std::fs::write(
            dir.path().join(".jj/repo/config.toml"),
            "[ui]\nconflict-marker-style = \"bogus\"\n",
        )
        .unwrap();
        assert_eq!(
            conflict_marker_style_for_repo(dir.path()),
            ConflictMarkerStyle::Diff,
            "invalid value should fall back to the default"
        );
    }

    /// REPRO D (megamerge): @ is a 2-parent merge; a file was modified in only
    /// ONE leg (no conflict). Touching it on disk should be a no-op for stats.
    #[tokio::test]
    async fn repro_megamerge_single_leg_file_touch() {
        let dir = create_jj_repo().await;
        let config = Config {
            color: false,
            ..Default::default()
        };
        // base: two files
        std::fs::write(dir.path().join("a.txt"), "a1\na2\na3\n").unwrap();
        std::fs::write(dir.path().join("b.txt"), "b1\nb2\nb3\n").unwrap();
        jj_cmd(dir.path(), &["commit", "-m", "base"]).await;
        jj_cmd(dir.path(), &["bookmark", "create", "base-bm", "-r", "@-"]).await;
        // leg A modifies a.txt only
        std::fs::write(dir.path().join("a.txt"), "a1\nA2-CHANGED\na3\n").unwrap();
        jj_cmd(dir.path(), &["commit", "-m", "leg-a"]).await;
        jj_cmd(dir.path(), &["bookmark", "create", "leg-a-bm", "-r", "@-"]).await;
        // leg B modifies b.txt only
        jj_cmd(dir.path(), &["new", "base-bm"]).await;
        std::fs::write(dir.path().join("b.txt"), "b1\nB2-CHANGED\nb3\n").unwrap();
        jj_cmd(dir.path(), &["commit", "-m", "leg-b"]).await;
        jj_cmd(dir.path(), &["bookmark", "create", "leg-b-bm", "-r", "@-"]).await;
        // megamerge: @ = merge(leg-a, leg-b), no conflicts
        jj_cmd(dir.path(), &["new", "leg-a-bm", "leg-b-bm"]).await;

        let jj_worker = spawn_jj_worker();
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        jj_worker
            .send(JjWorkerRequest::FullRefresh {
                repo_path: dir.path().to_path_buf(),
                depth: config.bookmark_search_depth,
                reply: reply_tx,
            })
            .unwrap();
        let full = reply_rx.await.unwrap().unwrap();
        eprintln!(
            "full refresh: files={} +{} -{}",
            full.file_mad_count_working_tree,
            full.lines_added_working_tree,
            full.lines_removed_working_tree
        );

        // Touch a.txt (unchanged content — e.g. editor re-save / mtime bump)
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        jj_worker
            .send(JjWorkerRequest::IncrementalUpdate {
                repo_path: dir.path().to_path_buf(),
                changed_paths: vec![dir.path().join("a.txt")],
                reply: reply_tx,
            })
            .unwrap();
        let incr = reply_rx.await.unwrap().unwrap();
        eprintln!(
            "incremental after touching a.txt: files={} +{} -{}",
            incr.file_mad_count_working_tree,
            incr.lines_added_working_tree,
            incr.lines_removed_working_tree
        );
        assert_eq!(
            (
                incr.file_mad_count_working_tree,
                incr.lines_added_working_tree
            ),
            (0, 0),
            "touching an unmodified single-leg file in a megamerge should stay empty"
        );
    }

    /// Resync after event loss: stale overlay entries (whose files changed
    /// while events were dropped) are re-diffed from disk, and known dirty
    /// files survive the rebuild.
    #[tokio::test]
    async fn test_resync_heals_stale_overlay_and_keeps_dirty_files() {
        let dir = create_jj_repo().await;
        let config = Config {
            color: false,
            ..Default::default()
        };
        let jj_worker = spawn_jj_worker();
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        jj_worker
            .send(JjWorkerRequest::FullRefresh {
                repo_path: dir.path().to_path_buf(),
                depth: config.bookmark_search_depth,
                reply: reply_tx,
            })
            .unwrap();
        reply_rx.await.unwrap().unwrap();

        // Two dirty files delivered normally → both in the overlay
        std::fs::write(dir.path().join("keep.txt"), "a\nb\n").unwrap();
        std::fs::write(dir.path().join("stale.txt"), "x\ny\nz\n").unwrap();
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        jj_worker
            .send(JjWorkerRequest::IncrementalUpdate {
                repo_path: dir.path().to_path_buf(),
                changed_paths: vec![dir.path().join("keep.txt"), dir.path().join("stale.txt")],
                reply: reply_tx,
            })
            .unwrap();
        let status = reply_rx.await.unwrap().unwrap();
        assert_eq!(status.file_mad_count_working_tree, 2);
        assert_eq!(status.lines_added_working_tree, 5);

        // Event loss: stale.txt is deleted but the event never arrives
        std::fs::remove_file(dir.path().join("stale.txt")).unwrap();

        // Rescan-triggered resync must drop stale.txt and keep keep.txt
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        jj_worker
            .send(JjWorkerRequest::Resync {
                repo_path: dir.path().to_path_buf(),
                depth: config.bookmark_search_depth,
                reply: reply_tx,
            })
            .unwrap();
        let status = reply_rx.await.unwrap().unwrap();
        assert_eq!(
            (
                status.file_mad_count_working_tree,
                status.lines_added_working_tree
            ),
            (1, 2),
            "resync should re-diff known dirty files from disk"
        );
    }

    /// Consecutive full refreshes through the same worker: the second and
    /// third reuse the retained workspace loader and must still observe new
    /// operations (`RepoLoader::load_at_head` re-reads op heads from disk on
    /// every call).
    #[tokio::test]
    async fn test_retained_loader_sees_new_operations() {
        let dir = create_jj_repo().await;
        let config = Config {
            color: false,
            ..Default::default()
        };
        let jj_worker = spawn_jj_worker();

        let full_refresh = |reply| JjWorkerRequest::FullRefresh {
            repo_path: dir.path().to_path_buf(),
            depth: config.bookmark_search_depth,
            reply,
        };

        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        jj_worker.send(full_refresh(reply_tx)).unwrap();
        let first = reply_rx.await.unwrap().unwrap();

        // describe: new operation, same change
        jj_cmd(dir.path(), &["describe", "-m", "second op"]).await;
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        jj_worker.send(full_refresh(reply_tx)).unwrap();
        let second = reply_rx.await.unwrap().unwrap();
        assert_eq!(
            second.description, "second op",
            "refresh via retained loader must see the new operation"
        );

        // new: new operation AND new working-copy change
        jj_cmd(dir.path(), &["new"]).await;
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        jj_worker.send(full_refresh(reply_tx)).unwrap();
        let third = reply_rx.await.unwrap().unwrap();
        assert_ne!(
            third.change_id, first.change_id,
            "new working-copy change must be visible through the retained loader"
        );
        assert!(third.description.is_empty());
    }

    /// Router behavior: stats fan out across per-repo workers, Forget drops
    /// a repo's state, and the next request respawns a fresh worker.
    #[tokio::test]
    async fn test_worker_router_forget_and_stats() {
        let dir = create_jj_repo().await;
        let config = Config {
            color: false,
            ..Default::default()
        };
        let jj_worker = spawn_jj_worker();

        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        jj_worker
            .send(JjWorkerRequest::FullRefresh {
                repo_path: dir.path().to_path_buf(),
                depth: config.bookmark_search_depth,
                reply: reply_tx,
            })
            .unwrap();
        reply_rx.await.unwrap().unwrap();

        // Dirty a file so overlay stats have something to report
        std::fs::write(dir.path().join("f.txt"), "a\n").unwrap();
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        jj_worker
            .send(JjWorkerRequest::IncrementalUpdate {
                repo_path: dir.path().to_path_buf(),
                changed_paths: vec![dir.path().join("f.txt")],
                reply: reply_tx,
            })
            .unwrap();
        let status = reply_rx.await.unwrap().unwrap();
        assert_eq!(status.file_mad_count_working_tree, 1);

        let (tx, rx) = tokio::sync::oneshot::channel();
        jj_worker
            .send(JjWorkerRequest::QueryOverlayStats { reply: tx })
            .unwrap();
        let stats = rx.await.unwrap();
        assert_eq!(stats.len(), 1, "fan-out should gather the repo's stats");
        assert_eq!(stats[0].1.overlay_entries, 1);

        // Forget drops the worker and its state (router handles requests in
        // order, so the follow-up goes to a fresh worker with no state).
        jj_worker
            .send(JjWorkerRequest::Forget {
                repo_path: dir.path().to_path_buf(),
            })
            .unwrap();
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        jj_worker
            .send(JjWorkerRequest::IncrementalUpdate {
                repo_path: dir.path().to_path_buf(),
                changed_paths: vec![dir.path().join("f.txt")],
                reply: reply_tx,
            })
            .unwrap();
        assert!(
            reply_rx.await.unwrap().is_err(),
            "incremental state should be gone after Forget"
        );

        // The respawned worker works normally: a full refresh rebuilds state
        // (f.txt was never snapshotted, so the store-based diff is empty),
        // after which incremental updates land again.
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        jj_worker
            .send(JjWorkerRequest::FullRefresh {
                repo_path: dir.path().to_path_buf(),
                depth: config.bookmark_search_depth,
                reply: reply_tx,
            })
            .unwrap();
        let status = reply_rx.await.unwrap().unwrap();
        assert_eq!(status.file_mad_count_working_tree, 0);

        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        jj_worker
            .send(JjWorkerRequest::IncrementalUpdate {
                repo_path: dir.path().to_path_buf(),
                changed_paths: vec![dir.path().join("f.txt")],
                reply: reply_tx,
            })
            .unwrap();
        let status = reply_rx.await.unwrap().unwrap();
        assert_eq!(status.file_mad_count_working_tree, 1);
    }

    /// A deleted file whose parent directories were also deleted must still
    /// map to a repo-relative path, even when the incoming path uses a
    /// non-canonical prefix (e.g. /var vs /private/var on macOS).
    #[test]
    fn test_abs_to_repo_relative_deleted_nested_dir() {
        let dir = tempfile::TempDir::new().unwrap();
        let canonical_root = dir.path().canonicalize().unwrap();
        // Never created — the whole gone/sub chain is missing
        let abs = dir.path().join("gone/sub/f.txt");
        assert_eq!(
            abs_to_repo_relative(&canonical_root, &abs).as_deref(),
            Some("gone/sub/f.txt")
        );
    }
}
