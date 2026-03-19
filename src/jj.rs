use anyhow::{Context, Result};
use futures::StreamExt;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;
use std::sync::Arc;
use jj_lib::backend::CommitId;
use jj_lib::backend::TreeValue;
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

use crate::config::Config;
use crate::template::{Bookmark, RepoStatus};

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


/// Read file content from the store into a Vec.
async fn read_file_content(
    store: &Arc<jj_lib::store::Store>,
    path: &jj_lib::repo_path::RepoPath,
    id: &jj_lib::backend::FileId,
) -> Option<Vec<u8>> {
    use tokio::io::AsyncReadExt;
    let mut reader = store.read_file(path, id).await.ok()?;
    let mut buf = Vec::new();
    reader.read_to_end(&mut buf).await.ok()?;
    Some(buf)
}

/// Check if content looks binary by scanning for null bytes in the first 8KB.
fn is_binary(content: &[u8]) -> bool {
    let check_len = content.len().min(8192);
    content[..check_len].contains(&0)
}

fn count_lines(content: &[u8]) -> u32 {
    bytecount::count(content, b'\n') as u32
}

/// Compute diff stats between two trees: (files_changed, lines_added, lines_removed).
///
/// For added/deleted files, counts newlines in the single version.
/// For modified files, uses jj-lib's line-level diff to count only actual
/// changed lines (not the full-replacement approximation).
/// Binary files are counted as 1 file changed but 0 lines.
#[tracing::instrument(skip_all)]
async fn compute_diff_stats(
    store: &Arc<jj_lib::store::Store>,
    from_tree: &jj_lib::merged_tree::MergedTree,
    to_tree: &jj_lib::merged_tree::MergedTree,
) -> (u32, u32, u32) {
    let mut files_changed = 0u32;
    let mut lines_added = 0u32;
    let mut lines_removed = 0u32;

    let mut diff_stream = from_tree.diff_stream(to_tree, &EverythingMatcher);
    while let Some(entry) = diff_stream.next().await {
        let Ok(values) = entry.values else {
            continue;
        };

        let before_file = values.before.as_normal().and_then(|tv| match tv {
            TreeValue::File { id, .. } => Some(id),
            _ => None,
        });
        let after_file = values.after.as_normal().and_then(|tv| match tv {
            TreeValue::File { id, .. } => Some(id),
            _ => None,
        });

        if before_file.is_none() && after_file.is_none() {
            continue;
        }

        files_changed += 1;

        match (before_file, after_file) {
            (None, Some(id)) => {
                // Added file: count lines in the new version
                if let Some(content) = read_file_content(store, &entry.path, id).await
                    && !is_binary(&content)
                {
                    lines_added += count_lines(&content);
                }
            }
            (Some(id), None) => {
                // Deleted file: count lines in the old version
                if let Some(content) = read_file_content(store, &entry.path, id).await
                    && !is_binary(&content)
                {
                    lines_removed += count_lines(&content);
                }
            }
            (Some(before_id), Some(after_id)) => {
                // Modified file: use line-level diff for accurate counts
                let before = read_file_content(store, &entry.path, before_id).await;
                let after = read_file_content(store, &entry.path, after_id).await;
                if let (Some(before), Some(after)) = (before, after)
                    && !is_binary(&before)
                    && !is_binary(&after)
                {
                    let diff = diff_by_line([&before, &after], &LineCompareMode::Exact);
                    for hunk in diff.hunks() {
                        if hunk.kind == DiffHunkKind::Different {
                            lines_removed += count_lines(hunk.contents[0].as_ref());
                            lines_added += count_lines(hunk.contents[1].as_ref());
                        }
                    }
                }
            }
            (None, None) => unreachable!(),
        }
    }

    (files_changed, lines_added, lines_removed)
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
        std::env::var("JJ_CONFIG").ok().map(std::path::PathBuf::from),
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
    let Ok(expression) = revset::parse(&mut diagnostics, "immutable_heads()::", &context) else {
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

    let mut queue: VecDeque<(CommitId, u32)> = VecDeque::new();
    let mut visited = HashSet::new();
    let mut seen_names = HashSet::new();
    let mut bookmarks = Vec::new();

    // Check bookmarks directly on the working copy commit (distance 0)
    if let Some(names) = bookmark_targets.get(wc_id) {
        for name_str in names {
            if seen_names.insert(name_str.clone()) {
                bookmarks.push(Bookmark {
                    name: name_str.clone(),
                    distance: 0,
                    display: name_str.clone(),
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
                    bookmarks.push(Bookmark {
                        name: name_str.clone(),
                        distance: depth,
                        display,
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

/// Core jj-lib query logic. This produces `!Send` futures (due to jj-lib internals),
/// so it must be run via `futures::executor::block_on` inside `spawn_blocking`.
#[tracing::instrument(fields(repo = %repo_path.display()))]
async fn query_jj_lib(repo_path: &Path, depth: u32) -> Result<RepoStatus> {
    let settings = create_user_settings()?;
    let workspace = {
        let _span = tracing::debug_span!("load_workspace").entered();
        Workspace::load(
            &settings,
            repo_path,
            &StoreFactories::default(),
            &default_working_copy_factories(),
        )
        .context("load jj workspace")?
    };

    let workspace_name = workspace.workspace_name().to_owned();
    let repo: Arc<jj_lib::repo::ReadonlyRepo> = {
        let _span = tracing::debug_span!("load_repo").entered();
        workspace
            .repo_loader()
            .load_at_head()
            .await
            .context("load jj repo at head")?
    };

    let view = repo.view();

    let wc_id = view
        .get_wc_commit_id(&workspace_name)
        .context("no working copy commit for workspace")?
        .clone();

    let commit = repo
        .store()
        .get_commit(&wc_id)
        .context("get working copy commit")?;

    let mut status = RepoStatus {
        is_jj: true,
        ..Default::default()
    };

    // Change ID (reverse hex, truncated to 8 chars)
    let change_id_full = encode_reverse_hex(commit.change_id().as_bytes());
    let id_len = 8.min(change_id_full.len());
    status.change_id = change_id_full[..id_len].to_string();

    // Commit ID (hex, truncated to 8 chars)
    let commit_id_hex = commit.id().hex();
    let id_len = 8.min(commit_id_hex.len());
    status.commit_id = commit_id_hex[..id_len].to_string();

    // Description (first line only)
    status.description = commit
        .description()
        .lines()
        .next()
        .unwrap_or("")
        .to_string();

    // Conflict
    status.conflict = commit.has_conflict();

    // Divergent
    status.divergent = repo
        .resolve_change_id(commit.change_id())
        .ok()
        .flatten()
        .is_some_and(|targets| targets.visible_with_offsets().count() > 1);

    // Hidden
    status.hidden = commit.is_hidden(repo.as_ref()).unwrap_or(false);

    // Immutable: check if commit is an ancestor of any immutable head
    // (trunk bookmarks, tags, untracked remote bookmarks).
    status.immutable = {
        let _span = tracing::debug_span!("check_immutable").entered();
        is_commit_immutable(&repo, &workspace_name, &wc_id)
    };

    // Bookmarks
    status.bookmarks = {
        let _span = tracing::debug_span!("find_bookmarks").entered();
        find_ancestor_bookmarks(&repo, view, &wc_id, depth)?
    };

    // Diff stats (also used to derive emptiness, avoiding a separate tree diff)
    let parent_tree = {
        let _span = tracing::debug_span!("load_parent_tree").entered();
        commit.parent_tree(repo.as_ref()).await.ok()
    };
    let current_tree = commit.tree();
    if let Some(ref parent_tree) = parent_tree {
        let (f, a, r) = compute_diff_stats(repo.store(), parent_tree, &current_tree).await;
        status.files_changed = f;
        status.lines_added = a;
        status.lines_removed = r;
        status.total_files_changed = f;
        status.total_lines_added = a;
        status.total_lines_removed = r;
        status.empty = f == 0;
    } else {
        status.empty = true;
    }

    // Workspace name
    status.workspace_name = workspace_name.as_str().to_string();
    status.is_default_workspace = status.workspace_name == "default";

    Ok(status)
}

#[tracing::instrument(skip(config), fields(repo = %repo_path.display()))]
pub async fn query_jj_status(
    repo_path: &Path,
    config: &Config,
) -> Result<RepoStatus> {
    let repo_path = repo_path.to_path_buf();
    let depth = config.bookmark_search_depth;

    // jj-lib async operations produce !Send futures (RefCell, OnceCell internals),
    // so we run them on a dedicated blocking thread. We use the tokio Handle
    // to block on the future so that tokio's IO reactor is available for
    // jj-lib's tokio::io::AsyncRead-based file reads.
    let handle = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || handle.block_on(query_jj_lib(&repo_path, depth)))
        .await
        .context("jj-lib task panicked")?
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use tokio::process::Command;

    async fn create_jj_repo() -> TempDir {
        let dir = TempDir::new().unwrap();
        let output = Command::new("jj")
            .args(["git", "init"])
            .current_dir(dir.path())
            .output()
            .await
            .unwrap();
        assert!(
            output.status.success(),
            "jj git init failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        dir
    }

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
        assert!(status.files_changed >= 1);
        assert!(status.lines_added > 0);
        // For jj, total should equal unstaged (no staging area)
        assert_eq!(status.total_files_changed, status.files_changed);
        assert_eq!(status.total_lines_added, status.lines_added);
        assert_eq!(status.total_lines_removed, status.lines_removed);
        assert_eq!(status.staged_files_changed, 0);
    }

    /// Parse the summary line from `diff --stat` output.
    /// Handles: " 3 files changed, 10 insertions(+), 5 deletions(-)"
    fn parse_diff_stat_summary(output: &str) -> (u32, u32, u32) {
        let Some(summary) = output.lines().rev().find(|l| l.contains("changed")) else {
            return (0, 0, 0);
        };
        let mut files = 0u32;
        let mut insertions = 0u32;
        let mut deletions = 0u32;
        for part in summary.split(',') {
            let part = part.trim();
            if part.contains("changed") {
                files = part
                    .split_whitespace()
                    .next()
                    .and_then(|n| n.parse().ok())
                    .unwrap_or(0);
            } else if part.contains("insertion") {
                insertions = part
                    .split_whitespace()
                    .next()
                    .and_then(|n| n.parse().ok())
                    .unwrap_or(0);
            } else if part.contains("deletion") {
                deletions = part
                    .split_whitespace()
                    .next()
                    .and_then(|n| n.parse().ok())
                    .unwrap_or(0);
            }
        }
        (files, insertions, deletions)
    }

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
        let lib_content: String = (1..=15).map(|i| format!("pub fn lib_{i}() {{}}\n")).collect();
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
        let test_initial: String =
            (1..=12).map(|i| format!("#[test] fn test_{i}() {{}}\n")).collect();
        std::fs::write(dir.path().join("tests/test_basic.rs"), &test_initial).unwrap();

        // Commit these as the parent: `jj new` moves @ forward
        jj_cmd(dir.path(), &["new"]).await;

        // --- Complex modifications ---

        // 1. New file: src/utils.rs (8 lines)
        let utils_content: String = (1..=8).map(|i| format!("pub fn util_{i}() {{}}\n")).collect();
        std::fs::write(dir.path().join("src/utils.rs"), &utils_content).unwrap();

        // 2. Delete tests/test_basic.rs
        std::fs::remove_file(dir.path().join("tests/test_basic.rs")).unwrap();

        // 3. Modify src/main.rs: change lines 5-7, add 4 lines at end
        let mut main_lines: Vec<String> =
            (1..=20).map(|i| format!("fn line_{i}() {{}}")).collect();
        main_lines[4] = "fn modified_5() { /* changed */ }".to_string();
        main_lines[5] = "fn modified_6() { /* changed */ }".to_string();
        main_lines[6] = "fn modified_7() { /* changed */ }".to_string();
        main_lines.push("fn added_21() {}".to_string());
        main_lines.push("fn added_22() {}".to_string());
        main_lines.push("fn added_23() {}".to_string());
        main_lines.push("fn added_24() {}".to_string());
        std::fs::write(
            dir.path().join("src/main.rs"),
            main_lines.join("\n") + "\n",
        )
        .unwrap();

        // 4. Modify README.md: remove last 3 lines, add 5 new lines
        let mut readme_lines: Vec<String> = (1..=7).map(|i| format!("# Section {i}")).collect();
        readme_lines.push("# New Section A".to_string());
        readme_lines.push("# New Section B".to_string());
        readme_lines.push("# New Section C".to_string());
        readme_lines.push("# New Section D".to_string());
        readme_lines.push("# New Section E".to_string());
        std::fs::write(
            dir.path().join("README.md"),
            readme_lines.join("\n") + "\n",
        )
        .unwrap();

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
            (status.files_changed, status.lines_added, status.lines_removed),
            (cli_files, cli_added, cli_removed),
            "our stats ({}f, +{}, -{}) != jj diff --stat ({}f, +{}, -{})\njj output:\n{}",
            status.files_changed,
            status.lines_added,
            status.lines_removed,
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
                5..=8 => continue,    // deleted
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
            (status.files_changed, status.lines_added, status.lines_removed),
            (cli_files, cli_added, cli_removed),
            "our stats ({}f, +{}, -{}) != jj diff --stat ({}f, +{}, -{})\njj output:\n{}",
            status.files_changed,
            status.lines_added,
            status.lines_removed,
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
        std::fs::write(
            dir.path().join("shrink.txt"),
            "aaa\nbbb\nccc\nddd\neee\n",
        )
        .unwrap();

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
            (status.files_changed, status.lines_added, status.lines_removed),
            (cli_files, cli_added, cli_removed),
            "our stats ({}f, +{}, -{}) != jj diff --stat ({}f, +{}, -{})\njj output:\n{}",
            status.files_changed,
            status.lines_added,
            status.lines_removed,
            cli_files,
            cli_added,
            cli_removed,
            jj_output,
        );
    }
}
