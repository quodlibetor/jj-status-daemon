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
    DaemonStatus,
    Version,
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

/// Performance statistics collected by the daemon.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct DaemonStats {
    /// Total queries received
    pub queries: u64,
    /// Cache hits (immediate response)
    pub cache_hits: u64,
    /// Cache misses (background populate)
    pub cache_misses: u64,
    /// Total refresh cycles (watcher-triggered re-queries)
    pub refreshes: u64,
    /// Total filesystem events received
    pub fs_events: u64,
    /// Filesystem events skipped because all paths were ignored
    pub fs_events_ignored: u64,
    /// Recent query durations in milliseconds (ring buffer, most recent last)
    pub recent_query_ms: Vec<f64>,
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
