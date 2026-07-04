use anyhow::{Context, Result};
use futures::StreamExt;
use jj_lib::backend::CommitId;
use jj_lib::config::{ConfigLayer, ConfigSource, StackedConfig};
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
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::mpsc;

use crate::config::Config;
use crate::template::{Bookmark, RepoStatus, TrackingStatus};

/// Create minimal UserSettings for read-only operations.
fn create_user_settings() -> Result<UserSettings> {
    let mut config = StackedConfig::with_defaults();
    let mut user_layer = ConfigLayer::empty(ConfigSource::User);
    user_layer
        .set_value("user.name", "vcs-status-daemon")
        .context("set user.name")?;
    user_layer
        .set_value("user.email", "vcs-status-daemon@localhost")
        .context("set user.email")?;
    config.add_layer(user_layer);
    UserSettings::from_config(config).context("create UserSettings")
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
) -> Option<Vec<u8>> {
    use jj_lib::conflicts::{
        ConflictMarkerStyle, ConflictMaterializeOptions, MaterializedTreeValue,
        choose_materialized_conflict_marker_len, materialize_merge_result_to_bytes,
        materialize_tree_value,
    };

    match materialize_tree_value(store, path, value, labels)
        .await
        .ok()?
    {
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
                // jj's default `ui.conflict-marker-style`
                marker_style: ConflictMarkerStyle::Diff,
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
    /// Tree IDs of the working-copy commit itself. When an op changes these
    /// without changing the parent (snapshot, abandon, edit-to-sibling,
    /// restore), the base stats must be rebuilt from the store — watcher
    /// events alone cannot be relied on to repair them.
    commit_tree_ids: jj_lib::merge::Merge<jj_lib::backend::TreeId>,
    /// Operation ID at the time this state was built. Tree ID comparison
    /// alone misses A→B→A sequences (e.g. `jj abandon` snapshots a dirty
    /// file into @ and then discards it — both trees end up as they
    /// started, but the op rewrote the working copy on disk). When the op
    /// advanced with unchanged trees, overlay entries must be re-diffed.
    op_id: jj_lib::op_store::OperationId,
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
            Some(Some(overlay_stats)) => tally(&mut counts, overlay_stats),
            Some(None) => {
                // File reverted to parent — excluded from diff
            }
            None => tally(&mut counts, stats),
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

/// Compute per-file diff stats between two trees.
#[tracing::instrument(skip_all)]
async fn compute_per_file_diff_stats(
    store: &Arc<jj_lib::store::Store>,
    from_tree: &jj_lib::merged_tree::MergedTree,
    to_tree: &jj_lib::merged_tree::MergedTree,
) -> HashMap<String, FileDiffStats> {
    let mut result = HashMap::new();

    let mut diff_stream = from_tree.diff_stream(to_tree, &EverythingMatcher);
    while let Some(entry) = diff_stream.next().await {
        let Ok(values) = entry.values else {
            continue;
        };

        let before =
            materialized_content(store, &entry.path, values.before, from_tree.labels()).await;
        let after = materialized_content(store, &entry.path, values.after, to_tree.labels()).await;

        let Some(stats) = diff_stats_for_contents(before.as_deref(), after.as_deref()) else {
            continue;
        };

        result.insert(entry.path.as_internal_file_string().to_string(), stats);
    }

    result
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
            if !is_binary(content) {
                stats.lines_added = count_lines(content);
            }
        }
        (Some(content), None) => {
            stats.kind = FileChangeKind::Deleted;
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
) -> Option<FileDiffStats> {
    // Materialize the parent-side value the same way jj does (conflicted
    // files become conflict-marker text, matching what jj writes to disk),
    // so an untouched conflicted file diffs as unchanged.
    let parent_value = parent_tree.path_value(repo_path).ok()?;
    let parent_content =
        materialized_content(store, repo_path, parent_value, parent_tree.labels()).await;

    diff_stats_for_contents(parent_content.as_deref(), disk_content)
}

/// Diff each changed path on disk against the parent tree and record the
/// results in the overlay. Paths are deduplicated — each is re-read from
/// disk at processing time, so duplicates are pure wasted work.
async fn apply_incremental_paths(state: &mut JjRepoState, changed_paths: &[PathBuf]) {
    let mut seen = HashSet::new();
    for abs_path in changed_paths {
        if !seen.insert(abs_path) {
            continue;
        }
        let Some(rel_str) = abs_to_repo_relative(&state.repo_root, abs_path) else {
            continue;
        };
        let Ok(repo_path_buf) = jj_lib::repo_path::RepoPathBuf::from_relative_path(&rel_str) else {
            continue;
        };
        // Read file from disk (None if deleted/missing)
        let disk_content = std::fs::read(abs_path).ok();
        let diff_result = diff_single_file(
            &state.store,
            &state.parent_tree,
            &repo_path_buf,
            disk_content.as_deref(),
        )
        .await;
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
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(table) = content.parse::<toml::Table>() else {
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

/// Check if a commit is immutable by evaluating the `immutable_heads()::` revset.
///
/// This uses jj's revset engine with the same default aliases as jj-cli,
/// plus any user overrides from their jj config files.
fn is_commit_immutable(
    repo: &Arc<jj_lib::repo::ReadonlyRepo>,
    workspace_name: &WorkspaceName,
    commit_id: &CommitId,
) -> bool {
    // Build aliases map with defaults from jj-cli
    let mut aliases_map = RevsetAliasesMap::new();
    let _ = aliases_map.insert("trunk()", DEFAULT_TRUNK_ALIAS);
    let _ = aliases_map.insert(
        "builtin_immutable_heads()",
        DEFAULT_BUILTIN_IMMUTABLE_HEADS_ALIAS,
    );
    let _ = aliases_map.insert("immutable_heads()", DEFAULT_IMMUTABLE_HEADS_ALIAS);

    // Load user overrides (e.g. custom immutable_heads())
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
    let Ok(expression) = revset::parse(&mut diagnostics, "::immutable_heads()", &context) else {
        return false;
    };

    let symbol_resolver = SymbolResolver::new(repo.as_ref(), extensions.symbol_resolvers());
    let Ok(resolved) = expression.resolve_user_expression(repo.as_ref(), &symbol_resolver) else {
        return false;
    };

    let Ok(revset) = resolved.evaluate(repo.as_ref()) else {
        return false;
    };

    let containing = revset.containing_fn();
    containing(commit_id).unwrap_or(false)
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

/// Build a map of bookmark name → tracking status by scanning all tracked remote refs.
fn compute_tracking_statuses(
    repo: &Arc<jj_lib::repo::ReadonlyRepo>,
    view: &jj_lib::view::View,
) -> HashMap<String, TrackingStatus> {
    let mut result: HashMap<String, TrackingStatus> = HashMap::new();

    for (symbol, remote_ref) in view.all_remote_bookmarks() {
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

    // Compute tracking statuses for all bookmarks against their remotes
    let tracking_statuses = compute_tracking_statuses(repo, view);

    let mut queue: VecDeque<(CommitId, u32)> = VecDeque::new();
    let mut visited = HashSet::new();
    let mut seen_names = HashSet::new();
    let mut bookmarks = Vec::new();

    // Check bookmarks directly on the working copy commit (distance 0)
    if let Some(names) = bookmark_targets.get(wc_id) {
        for name_str in names {
            if seen_names.insert(name_str.clone()) {
                let tracking = tracking_statuses.get(name_str).cloned().unwrap_or_default();
                bookmarks.push(Bookmark {
                    name: name_str.clone(),
                    distance: 0,
                    display: name_str.clone(),
                    tracking,
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
        if depth > max_depth || !visited.insert(commit_id.clone()) {
            continue;
        }

        if let Some(names) = bookmark_targets.get(&commit_id) {
            for name_str in names {
                if seen_names.insert(name_str.clone()) {
                    let display = format!("{name_str}+{depth}");
                    let tracking = tracking_statuses.get(name_str).cloned().unwrap_or_default();
                    bookmarks.push(Bookmark {
                        name: name_str.clone(),
                        distance: depth,
                        display,
                        tracking,
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
    parent_tree_ids: jj_lib::merge::Merge<jj_lib::backend::TreeId>,
    /// Metadata-only status (no diff stats populated).
    metadata_status: RepoStatus,
}

/// Load a jj workspace/repo and compute metadata. This is the shared first step
/// for both full refresh and validate-and-refresh — call once, then branch on
/// whether the parent tree IDs changed.
async fn load_jj_repo(repo_path: &Path, depth: u32) -> Result<JjLoadedRepo> {
    let settings = create_user_settings()?;
    let workspace = Workspace::load(
        &settings,
        repo_path,
        &StoreFactories::default(),
        &default_working_copy_factories(),
    )
    .context("load jj workspace")?;

    let workspace_name = workspace.workspace_name().to_owned();
    let repo: Arc<jj_lib::repo::ReadonlyRepo> = workspace
        .repo_loader()
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
        .unwrap_or_else(|| commit.tree().tree_ids().clone());

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
    status.bookmarks = find_ancestor_bookmarks(&repo, view, &wc_id, depth)?;
    status.workspace_name = workspace_name.as_str().to_string();
    status.is_default_workspace = status.workspace_name == "default";

    Ok(JjLoadedRepo {
        repo,
        commit,
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

    let parent_tree = {
        let _span = tracing::debug_span!("load_parent_tree").entered();
        loaded.commit.parent_tree(loaded.repo.as_ref()).await.ok()
    };
    let current_tree = loaded.commit.tree();
    let base_file_stats = if let Some(ref parent_tree) = parent_tree {
        let per_file =
            compute_per_file_diff_stats(loaded.repo.store(), parent_tree, &current_tree).await;
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

    let commit_tree_ids = current_tree.tree_ids().clone();
    let retained_parent_tree = parent_tree.unwrap_or(current_tree);

    let jj_state = JjRepoState {
        store: loaded.repo.store().clone(),
        parent_tree_ids: retained_parent_tree.tree_ids().clone(),
        parent_tree: retained_parent_tree,
        commit_tree_ids,
        op_id: loaded.repo.op_id().clone(),
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
}

/// Spawn a dedicated blocking thread that owns !Send jj-lib state.
///
/// Returns a sender for submitting requests. The worker exits when the sender is dropped.
pub fn spawn_jj_worker() -> mpsc::UnboundedSender<JjWorkerRequest> {
    let (tx, rx) = mpsc::unbounded_channel();
    let handle = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || {
        handle.block_on(jj_worker_loop(rx));
    });
    tx
}

async fn jj_worker_loop(mut rx: mpsc::UnboundedReceiver<JjWorkerRequest>) {
    let mut states: HashMap<PathBuf, JjRepoState> = HashMap::new();

    while let Some(req) = rx.recv().await {
        match req {
            JjWorkerRequest::FullRefresh {
                repo_path,
                depth,
                reply,
            } => {
                let result = query_jj_lib(&repo_path, depth).await;
                match result {
                    Ok((status, jj_state)) => {
                        states.insert(repo_path, jj_state);
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
                let Some(state) = states.get_mut(&repo_path) else {
                    // No retained state — fall through to full refresh
                    let result = query_jj_lib(&repo_path, depth).await;
                    match result {
                        Ok((status, jj_state)) => {
                            states.insert(repo_path, jj_state);
                            let _ = reply.send(Ok(status));
                        }
                        Err(e) => {
                            let _ = reply.send(Err(e));
                        }
                    }
                    continue;
                };

                // Load workspace/repo once — then branch on parent tree IDs
                let loaded = match load_jj_repo(&repo_path, depth).await {
                    Ok(l) => l,
                    Err(e) => {
                        let _ = reply.send(Err(e));
                        continue;
                    }
                };

                let trees_unchanged = loaded.parent_tree_ids == state.parent_tree_ids
                    && loaded.commit.tree().tree_ids() == &state.commit_tree_ids;

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
                    let result = compute_jj_full_status(&repo_path, loaded).await;
                    match result {
                        Ok((_, mut jj_state)) => {
                            apply_incremental_paths(&mut jj_state, &changed_paths).await;
                            let status = jj_state.current_status();
                            states.insert(repo_path, jj_state);
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
                let Some(state) = states.get_mut(&repo_path) else {
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
                let prior_dirty: Vec<PathBuf> = states
                    .get(&repo_path)
                    .map(|s| s.overlay.keys().map(|rel| s.repo_root.join(rel)).collect())
                    .unwrap_or_default();
                match query_jj_lib(&repo_path, depth).await {
                    Ok((_, mut jj_state)) => {
                        apply_incremental_paths(&mut jj_state, &prior_dirty).await;
                        let status = jj_state.current_status();
                        states.insert(repo_path, jj_state);
                        let _ = reply.send(Ok(status));
                    }
                    Err(e) => {
                        let _ = reply.send(Err(e));
                    }
                }
            }
            JjWorkerRequest::QueryOverlayStats { reply } => {
                let stats: Vec<_> = states
                    .iter()
                    .map(|(path, state)| {
                        let counts = state.aggregate_stats();
                        (
                            path.to_string_lossy().to_string(),
                            crate::protocol::IncrementalDiffStats {
                                base_files: state.base_file_stats.len() as u32,
                                overlay_entries: state.overlay.len() as u32,
                                files_changed: counts.file_mad_count,
                                lines_added: counts.lines_added,
                                lines_removed: counts.lines_removed,
                            },
                        )
                    })
                    .collect();
                let _ = reply.send(stats);
            }
            JjWorkerRequest::QueryOverlayStatsVerbose { reply } => {
                let stats: Vec<_> = states
                    .iter()
                    .map(|(path, state)| {
                        let dir_stats =
                            aggregate_overlay_stats_by_dir(&state.base_file_stats, &state.overlay);
                        (path.to_string_lossy().to_string(), dir_stats)
                    })
                    .collect();
                let _ = reply.send(stats);
            }
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
