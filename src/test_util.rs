use std::path::Path;
use tempfile::TempDir;

/// Async wrapper: create a jj repo via spawn_blocking.
pub async fn create_jj_repo_async() -> TempDir {
    tokio::task::spawn_blocking(create_jj_repo).await.unwrap()
}

/// Async wrapper: create a git repo via spawn_blocking.
pub async fn create_git_repo_async() -> TempDir {
    tokio::task::spawn_blocking(create_git_repo).await.unwrap()
}

/// Wait for a Unix socket file to appear (polls every 5ms, up to 10s).
pub async fn wait_for_socket(socket_path: &Path) {
    for _ in 0..2000 {
        if socket_path.exists() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    panic!("socket never appeared at {}", socket_path.display());
}

/// Parse a `git diff --stat` / `jj diff --stat` summary line into (files, insertions, deletions).
pub fn parse_diff_stat_summary(output: &str) -> (u32, u32, u32) {
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

/// Removes the external per-repo jj config dir (`<config root>/jj/repos/<id>`)
/// that `jj config set --repo` creates, so tests that set repo config don't
/// litter the user's config directory. Runs on drop, including panics.
pub struct RepoConfigCleanup {
    repo_root: std::path::PathBuf,
}

impl RepoConfigCleanup {
    pub fn new(repo_root: &Path) -> Self {
        RepoConfigCleanup {
            repo_root: repo_root.to_path_buf(),
        }
    }
}

impl Drop for RepoConfigCleanup {
    fn drop(&mut self) {
        let Ok(config_id) = std::fs::read_to_string(self.repo_root.join(".jj/repo/config-id"))
        else {
            return;
        };
        let config_id = config_id.trim();
        if config_id.is_empty() || !config_id.chars().all(|c| c.is_ascii_hexdigit()) {
            return;
        }
        for root in crate::jj::jj_config_roots() {
            let _ = std::fs::remove_dir_all(root.join("repos").join(config_id));
        }
    }
}

/// Create a jj repo (colocated with git) using jj-lib directly — no CLI subprocess.
pub fn create_jj_repo() -> TempDir {
    use jj_lib::config::{ConfigLayer, ConfigSource, StackedConfig};
    use jj_lib::settings::UserSettings;
    use jj_lib::workspace::Workspace;

    let dir = TempDir::new().unwrap();
    let mut config = StackedConfig::with_defaults();
    let mut layer = ConfigLayer::empty(ConfigSource::User);
    layer.set_value("user.name", "Test").unwrap();
    layer.set_value("user.email", "test@test").unwrap();
    config.add_layer(layer);
    let settings = UserSettings::from_config(config).unwrap();
    // Use pollster for a minimal block_on — avoids requiring an async runtime,
    // which matters for spawn_blocking contexts and sync tests.
    pollster::block_on(Workspace::init_colocated_git(&settings, dir.path())).unwrap();
    dir
}

/// Create a git repo with an initial commit using git2 — no CLI subprocess.
pub fn create_git_repo() -> TempDir {
    let dir = TempDir::new().unwrap();
    create_git_repo_in(dir.path());
    dir
}

/// Initialise a git repo with an initial commit at `path` using git2.
pub fn create_git_repo_in(path: &Path) {
    let repo = git2::Repository::init(path).unwrap();
    {
        let mut config = repo.config().unwrap();
        config.set_str("user.email", "test@test.com").unwrap();
        config.set_str("user.name", "Test").unwrap();
    }
    std::fs::write(path.join("README"), "init\n").unwrap();
    let mut index = repo.index().unwrap();
    index.add_path(std::path::Path::new("README")).unwrap();
    index.write().unwrap();
    let tree_oid = index.write_tree().unwrap();
    let tree = repo.find_tree(tree_oid).unwrap();
    let sig = repo.signature().unwrap();
    repo.commit(Some("HEAD"), &sig, &sig, "initial", &tree, &[])
        .unwrap();
}
