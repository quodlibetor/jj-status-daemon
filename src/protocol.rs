use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VcsKind {
    Jj,
    Git,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum Request {
    Query {
        repo_path: String,
        timeout_override_ms: u64,
    },
    Flush,
    ReloadConfig,
    Shutdown,
    DaemonStatus {
        #[serde(default)]
        verbose: bool,
    },
    Version,
    SetLogFilter {
        filter: String,
    },
}

impl Request {
    #[cfg(test)]
    pub fn test_query(repo_path: impl Into<String>) -> Request {
        Request::Query {
            repo_path: repo_path.into(),
            timeout_override_ms: 2_000,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum Response {
    Status {
        formatted: String,
    },
    NotReady {
        formatted: String,
    },
    Error {
        message: String,
    },
    Ok,
    DaemonStatus {
        pid: u32,
        uptime_secs: u64,
        watched_repos: Vec<String>,
        stats: DaemonStats,
        /// Per-repo incremental diff overlay stats (repo path → stats).
        #[serde(default)]
        incremental_diff_stats: Vec<(String, IncrementalDiffStats)>,
        /// Per-repo per-directory breakdown (only populated when verbose=true).
        /// Outer vec: (repo_path, vec of (dir_path, stats)).
        #[serde(default)]
        dir_diff_stats: Vec<(String, Vec<(String, IncrementalDiffStats)>)>,
    },
    Version {
        version: String,
        git_hash: String,
        features: Vec<String>,
    },
}

/// Build-time version info.
pub fn version_info() -> (String, String, Vec<String>) {
    let version = env!("CARGO_PKG_VERSION").to_string();
    let git_hash = env!("VSD_GIT_HASH").to_string();
    let mut features = Vec::new();
    if cfg!(feature = "tokio-console") {
        features.push("tokio-console".to_string());
    }
    (version, git_hash, features)
}

/// Per-repo per-directory verbose stats: `Vec<(repo_path, Vec<(dir, stats)>)>`.
pub type VerboseDirStats = Vec<(String, Vec<(String, IncrementalDiffStats)>)>;

/// Per-repo incremental diff overlay statistics.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct IncrementalDiffStats {
    /// Files with diff stats from the last full refresh.
    pub base_files: u32,
    /// Files updated incrementally since the last full refresh (overlay entries).
    pub overlay_entries: u32,
    /// Current aggregated file count (files with changes).
    pub files_changed: u32,
    /// Current aggregated lines added.
    pub lines_added: u32,
    /// Current aggregated lines removed.
    pub lines_removed: u32,
}

/// Performance statistics collected by the daemon.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct DaemonStats {
    /// Total queries received
    pub queries: u64,
    /// Cache hits (immediate response)
    pub cache_hits: u64,
    /// Cache misses (background populate)
    pub cache_misses: u64,
    /// Full refresh cycles (VCS-internal events like commit/reset)
    pub full_refreshes: u64,
    /// Incremental refresh cycles (working-copy file changes)
    pub incremental_refreshes: u64,
    /// Total filesystem events received
    pub fs_events: u64,
    /// Filesystem events skipped because all paths were ignored
    pub fs_events_ignored: u64,
    /// Recent query durations in milliseconds (ring buffer, most recent last)
    pub recent_query_ms: Vec<f64>,
    /// Recent full-refresh durations in milliseconds (ring buffer, most recent last)
    pub recent_full_refresh_ms: Vec<f64>,
    /// Recent incremental-refresh durations in milliseconds (ring buffer, most recent last)
    pub recent_incremental_refresh_ms: Vec<f64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_request_roundtrip() {
        let query = Request::test_query("repo_path");
        let json = serde_json::to_string(&query).unwrap();
        let parsed: Request = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, query);

        let shutdown = Request::Shutdown;
        let json = serde_json::to_string(&shutdown).unwrap();
        let parsed: Request = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, shutdown);
    }

    #[test]
    fn test_response_roundtrip() {
        let status = Response::Status {
            formatted: "abc123 main [1 +5-2] ".to_string(),
        };
        let json = serde_json::to_string(&status).unwrap();
        let parsed: Response = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, status);

        let error = Response::Error {
            message: "not found".to_string(),
        };
        let json = serde_json::to_string(&error).unwrap();
        let parsed: Response = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, error);

        let ok = Response::Ok;
        let json = serde_json::to_string(&ok).unwrap();
        let parsed: Response = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, ok);
    }
}
